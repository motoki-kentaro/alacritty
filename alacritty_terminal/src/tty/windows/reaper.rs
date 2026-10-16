//! Last-resort reaping of the shell that was spawned directly into a pseudoconsole.
//!
//! Closing the pseudoconsole is the normal way a shell ends: the console host sends the attached
//! clients a close event. A shell whose console initialization has not finished yet can miss that
//! event, stay alive forever and keep its headless console host alive with it. [`ShellReaper`]
//! holds its own handle to the shell and, only if the shell is still alive after the pseudoconsole
//! was closed, gives it a short grace period and then terminates it.
//!
//! Only the process this module was given a handle for is ever terminated. Holding the handle
//! keeps the PID reserved, so a recycled PID can never be hit. Processes the shell started are
//! not touched.

use std::io::Error;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use log::{debug, warn};
use windows_sys::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessId, TerminateProcess, WaitForSingleObject,
};

/// How long a shell gets to leave by itself after the pseudoconsole was closed.
///
/// An initial value measured on a few machines, not a universal optimum.
pub(super) const EXIT_GRACE: Duration = Duration::from_millis(500);

/// How long to wait for the exit to show up after `TerminateProcess` (which is asynchronous).
const TERMINATE_CONFIRM: Duration = Duration::from_secs(2);

/// Grace period and confirmation used when the reaper thread could not be created and the shell
/// has to be reaped on the thread that drops the `Pty`. Kept short because that thread may be one
/// that must not stall; it bounds the stall to 600 ms, at the price of terminating a slow shell
/// sooner than usual. Only reachable when the OS refuses to create a thread.
const FALLBACK_GRACE: Duration = Duration::from_millis(100);
const FALLBACK_CONFIRM: Duration = Duration::from_millis(500);

/// Number of `Pty`s whose shell has not been fully dealt with yet: created, and not yet either
/// dropped-with-the-shell-gone or reaped. See [`wait_for_pending_shells`].
static PENDING: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

/// Number of shells that could not be confirmed gone after reaping (`TerminateProcess` failed, the
/// exit was not observed in time, or the wait failed).
static UNRESOLVED: AtomicUsize = AtomicUsize::new(0);

/// Reaps `handle` and records a shell that could not be confirmed gone.
fn reap_and_record(handle: OwnedHandle, grace: Duration, confirm: Duration) {
    if !matches!(reap(handle, grace, confirm), Outcome::Exited | Outcome::Terminated) {
        UNRESOLVED.fetch_add(1, Ordering::SeqCst);
    }
}

/// Counts one shell as pending until dropped.
struct PendingToken;

impl PendingToken {
    fn new() -> Self {
        *PENDING.0.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        PendingToken
    }
}

impl Drop for PendingToken {
    fn drop(&mut self) {
        let mut count = PENDING.0.lock().unwrap_or_else(|e| e.into_inner());
        *count = count.saturating_sub(1);
        if *count == 0 {
            PENDING.1.notify_all();
        }
    }
}

/// Result of [`wait_for_pending_shells`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellCleanup {
    /// Every shell is gone.
    Complete,
    /// `timeout` elapsed while `pending` shells were still being dealt with (their `Pty` was not
    /// dropped yet, or the reaper had not finished). `unresolved` is the number of shells that
    /// could not be confirmed gone before.
    TimedOut { pending: usize, unresolved: usize },
    /// Nothing is pending any more, but `unresolved` shells could not be confirmed gone
    /// (`TerminateProcess` failed or the exit was not observed in time, or the wait failed).
    Unconfirmed { unresolved: usize },
}

impl ShellCleanup {
    /// Whether every shell is known to be gone.
    pub fn is_complete(self) -> bool {
        self == ShellCleanup::Complete
    }
}

/// Blocks until every shell spawned through this module has been dealt with: its `Pty` was
/// dropped (so the pseudoconsole was closed) and the shell is gone, terminated if it had to be.
/// A shell counts as pending from before its process is created, so a spawn that races with the
/// call is never overlooked.
///
/// Returns [`ShellCleanup::Complete`] only if everything is known to be gone; a timeout and a
/// shell that could not be confirmed gone are told apart, and both are logged (warn) with the
/// counts.
///
/// A process that exits right after dropping its terminals ends the detached threads that close
/// the pseudoconsoles and reap the shells, which can leave a shell behind. Call this once, at the
/// end of `main`, after every terminal was dropped. It returns immediately when nothing is
/// pending and never sleeps for a fixed time. Terminals that are still alive keep it waiting
/// until `timeout`.
pub fn wait_for_pending_shells(timeout: Duration) -> ShellCleanup {
    let deadline = Instant::now() + timeout;
    let mut count = PENDING.0.lock().unwrap_or_else(|e| e.into_inner());
    while *count > 0 {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            let (pending, unresolved) = (*count, UNRESOLVED.load(Ordering::SeqCst));
            warn!(
                "{pending} ConPTY shell(s) still pending after {}ms ({unresolved} could not be                  confirmed gone)",
                timeout.as_millis()
            );
            return ShellCleanup::TimedOut { pending, unresolved };
        }
        count = PENDING.1.wait_timeout(count, left).unwrap_or_else(|e| e.into_inner()).0;
    }
    drop(count);

    match UNRESOLVED.load(Ordering::SeqCst) {
        0 => ShellCleanup::Complete,
        unresolved => {
            warn!("{unresolved} ConPTY shell(s) could not be confirmed gone");
            ShellCleanup::Unconfirmed { unresolved }
        },
    }
}

/// Exit code of a shell that had to be terminated. Differs from a natural exit code of `0`.
const FORCED_EXIT_CODE: u32 = 1;

/// What happened to the shell. Only used for logging and tests.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// The shell exited by itself within the grace period.
    Exited,
    /// The shell was still alive after the grace period and exited after `TerminateProcess`.
    Terminated,
    /// `TerminateProcess` failed and the shell is still alive.
    TerminateFailed,
    /// `TerminateProcess` was accepted but the exit was not observed in time.
    TerminateUnconfirmed,
    /// Waiting on the handle failed, so the state of the shell is unknown.
    WaitFailed,
}

enum Wait {
    Exited,
    TimedOut,
    Failed(Error),
}

fn wait(handle: &OwnedHandle, timeout: Duration) -> Wait {
    // `INFINITE` is `u32::MAX`, which must never be produced by a finite timeout.
    let millis = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
    match unsafe { WaitForSingleObject(handle.as_raw_handle() as HANDLE, millis) } {
        WAIT_OBJECT_0 => Wait::Exited,
        WAIT_TIMEOUT => Wait::TimedOut,
        WAIT_FAILED => Wait::Failed(Error::last_os_error()),
        other => Wait::Failed(Error::other(format!("unexpected wait result {other:#x}"))),
    }
}

fn exit_code(handle: &OwnedHandle) -> Option<u32> {
    let mut code = 0;
    (unsafe { GetExitCodeProcess(handle.as_raw_handle() as HANDLE, &mut code) } != 0)
        .then_some(code)
}

/// Waits for the shell to exit on its own and terminates it if it does not. Blocks for at most
/// `grace + confirm`.
pub(super) fn reap(handle: OwnedHandle, grace: Duration, confirm: Duration) -> Outcome {
    let pid = unsafe { GetProcessId(handle.as_raw_handle() as HANDLE) };
    let started = Instant::now();

    match wait(&handle, grace) {
        Wait::Exited => {
            debug!(
                "ConPTY shell pid={pid} exited {}ms after the pseudoconsole closed (code {:?})",
                started.elapsed().as_millis(),
                exit_code(&handle)
            );
            return Outcome::Exited;
        },
        Wait::Failed(err) => {
            warn!("ConPTY shell pid={pid}: waiting for exit failed: {err}");
            return Outcome::WaitFailed;
        },
        Wait::TimedOut => (),
    }

    let waited = started.elapsed();
    let terminate_error =
        (unsafe { TerminateProcess(handle.as_raw_handle() as HANDLE, FORCED_EXIT_CODE) } == 0)
            .then(Error::last_os_error);

    // Termination is asynchronous (and a failed call may race with a natural exit), so the exit
    // is confirmed either way.
    match (wait(&handle, confirm), terminate_error) {
        (Wait::Exited, None) => {
            warn!(
                "ConPTY shell pid={pid} still alive {}ms after the pseudoconsole closed; terminated it",
                waited.as_millis()
            );
            Outcome::Terminated
        },
        (Wait::Exited, Some(err)) => {
            debug!("ConPTY shell pid={pid} exited while TerminateProcess failed: {err}");
            Outcome::Exited
        },
        (_, Some(err)) => {
            warn!(
                "ConPTY shell pid={pid} still alive {}ms after the pseudoconsole closed and \
                 TerminateProcess failed: {err}",
                waited.as_millis()
            );
            Outcome::TerminateFailed
        },
        (_, None) => {
            warn!(
                "ConPTY shell pid={pid} did not exit within {}ms after TerminateProcess",
                confirm.as_millis()
            );
            Outcome::TerminateUnconfirmed
        },
    }
}

/// A shell counted as pending before its process exists. Reserve it before `CreateProcessW`, so
/// that no window exists in which a shell is running but not counted; it is released if the
/// process is never created, and handed to the [`ShellReaper`] once it is.
pub(super) struct PendingShell(PendingToken);

impl PendingShell {
    pub(super) fn reserve() -> Self {
        PendingShell(PendingToken::new())
    }
}

/// What the reaper thread (or the fallback) takes over: the shell handle, and the pending count
/// for it, released only once the shell is dealt with.
struct Job {
    handle: OwnedHandle,
    _pending: PendingToken,
}

/// Owns a second handle to the directly spawned shell and reaps it on drop if needed.
///
/// It must be dropped after the pseudoconsole was closed (it is the last field of `Pty`), because
/// the graceful close is what normally ends the shell.
///
/// Dropping does not block, with one exception: if the OS cannot create the reaper thread, the
/// shell is reaped on the dropping thread, for at most [`FALLBACK_GRACE`] + [`FALLBACK_CONFIRM`].
pub(super) struct ShellReaper {
    handle: Option<OwnedHandle>,
    pending: Option<PendingToken>,
    grace: Duration,
}

impl ShellReaper {
    /// Duplicates `shell`. The caller keeps ownership of the original handle. `pending` was
    /// reserved before the process was created and is released when the shell is dealt with
    /// (immediately, if duplicating the handle fails).
    pub(super) fn new(shell: &OwnedHandle, pending: PendingShell) -> Result<Self, Error> {
        let handle = shell.try_clone()?;
        Ok(Self { handle: Some(handle), pending: Some(pending.0), grace: EXIT_GRACE })
    }
}

#[cfg(test)]
static FAIL_THREAD_SPAWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn spawn_worker(f: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    #[cfg(test)]
    if FAIL_THREAD_SPAWN.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(Error::other("injected thread creation failure"));
    }
    thread::Builder::new().name("ConPTY shell reaper".into()).spawn(f).map(|_| ())
}

impl Drop for ShellReaper {
    fn drop(&mut self) {
        let (Some(handle), Some(pending)) = (self.handle.take(), self.pending.take()) else {
            return;
        };

        // Common case: the shell is already gone, so there is nothing to do but release it.
        if matches!(wait(&handle, Duration::ZERO), Wait::Exited) {
            return;
        }

        // The job lives in a shared slot until a worker takes it, so that it is neither lost nor
        // left unattended if the thread cannot be created.
        let slot = Arc::new(Mutex::new(Some(Job { handle, _pending: pending })));
        let worker_slot = slot.clone();
        let grace = self.grace;
        let spawned = spawn_worker(move || {
            let job = worker_slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(job) = job {
                reap_and_record(job.handle, grace, TERMINATE_CONFIRM);
            }
        });

        if let Err(err) = spawned {
            warn!("Could not start the ConPTY shell reaper thread, reaping synchronously: {err}");
            let job = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(job) = job {
                reap_and_record(job.handle, FALLBACK_GRACE.min(grace), FALLBACK_CONFIRM);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Child, Command, Stdio};

    use super::*;

    fn spawn(program: &str, args: &[&str]) -> Child {
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    /// A reaper for `shell` with its pending count reserved, as `conpty::new` does.
    fn new_reaper(shell: &OwnedHandle) -> ShellReaper {
        ShellReaper::new(shell, PendingShell::reserve()).unwrap()
    }

    fn long_running() -> Child {
        spawn("ping", &["-n", "60", "127.0.0.1"])
    }

    /// A live process is terminated after the grace period, and with the dedicated exit code.
    #[test]
    fn live_process_is_terminated_after_grace() {
        let _serial =
            super::super::child::HANDLE_COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let child = long_running();
        let observer = child.try_clone_handle();
        let start = Instant::now();
        let outcome =
            reap(OwnedHandle::from(child), Duration::from_millis(200), Duration::from_secs(5));
        let elapsed = start.elapsed();

        assert_eq!(outcome, Outcome::Terminated);
        assert!(elapsed >= Duration::from_millis(200), "terminated before the grace: {elapsed:?}");
        assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
        assert_eq!(exit_code(&observer), Some(FORCED_EXIT_CODE));
    }

    /// A process that leaves within the grace period is left alone and keeps its exit code.
    #[test]
    fn exiting_process_is_not_terminated() {
        let _serial =
            super::super::child::HANDLE_COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let child = spawn("cmd", &["/c", "exit 7"]);
        let observer = child.try_clone_handle();
        let outcome =
            reap(OwnedHandle::from(child), Duration::from_secs(5), Duration::from_secs(5));

        assert_eq!(outcome, Outcome::Exited);
        assert_eq!(exit_code(&observer), Some(7));
    }

    /// An already exited process returns right away, with no thread and no waiting.
    #[test]
    fn reaper_drop_after_exit_is_immediate() {
        let _serial =
            super::super::child::HANDLE_COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut child = spawn("cmd", &["/c", "exit 0"]);
        child.wait().unwrap();
        let reaper = new_reaper(&OwnedHandle::from(child));

        let start = Instant::now();
        drop(reaper);
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    /// Dropping the reaper for a live process returns at once; the worker terminates it later.
    #[test]
    fn reaper_drop_of_live_process_does_not_block() {
        let _serial =
            super::super::child::HANDLE_COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let child = long_running();
        let observer = child.try_clone_handle();
        let mut reaper = new_reaper(&OwnedHandle::from(child));
        reaper.grace = Duration::from_millis(300);

        let start = Instant::now();
        drop(reaper);
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "drop blocked: {:?}",
            start.elapsed()
        );

        assert!(matches!(wait(&observer, Duration::from_secs(5)), Wait::Exited));
        assert_eq!(exit_code(&observer), Some(FORCED_EXIT_CODE));
    }

    /// The reaper's handle outlives the original one (it is a real duplicate), and only the
    /// process it was given is affected.
    #[test]
    fn reaper_handle_is_independent_and_targeted() {
        let _serial =
            super::super::child::HANDLE_COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let target = long_running();
        let bystander = long_running();
        let target_observer = target.try_clone_handle();
        let bystander_observer = bystander.try_clone_handle();

        let original = OwnedHandle::from(target);
        let mut reaper = new_reaper(&original);
        reaper.grace = Duration::from_millis(100);
        drop(original);
        drop(reaper);

        assert!(matches!(wait(&target_observer, Duration::from_secs(5)), Wait::Exited));
        assert!(matches!(wait(&bystander_observer, Duration::ZERO), Wait::TimedOut));

        let mut bystander = bystander;
        bystander.kill().unwrap();
        bystander.wait().unwrap();
    }

    /// The pending count is held until the shell is gone, and released afterwards.
    #[test]
    fn pending_is_held_until_the_shell_is_reaped() {
        let _serial =
            super::super::child::HANDLE_COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            wait_for_pending_shells(Duration::from_secs(5)).is_complete(),
            "leftovers from another test"
        );

        let child = long_running();
        let observer = child.try_clone_handle();
        let mut reaper = new_reaper(&OwnedHandle::from(child));
        reaper.grace = Duration::from_millis(200);
        assert!(
            !wait_for_pending_shells(Duration::ZERO).is_complete(),
            "a live shell must count as pending"
        );

        drop(reaper);
        assert!(wait_for_pending_shells(Duration::from_secs(5)).is_complete());
        assert!(
            matches!(wait(&observer, Duration::ZERO), Wait::Exited),
            "returned before the shell was gone"
        );
        assert_eq!(exit_code(&observer), Some(FORCED_EXIT_CODE));
    }

    /// A reservation counts as pending before any process exists, and releases on drop; a wait
    /// that runs out reports how many shells it was still waiting for.
    #[test]
    fn reservation_is_pending_before_the_process_exists() {
        let _serial =
            super::super::child::HANDLE_COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(wait_for_pending_shells(Duration::from_secs(5)).is_complete());

        let reservation = PendingShell::reserve();
        match wait_for_pending_shells(Duration::from_millis(50)) {
            ShellCleanup::TimedOut { pending, .. } => assert_eq!(pending, 1),
            other => panic!("expected a timeout, got {other:?}"),
        }

        drop(reservation); // creation failed, or the shell is gone
        assert!(wait_for_pending_shells(Duration::ZERO).is_complete());
    }

    /// With nothing pending the wait returns at once instead of sleeping.
    #[test]
    fn waiting_with_nothing_pending_is_immediate() {
        let _serial =
            super::super::child::HANDLE_COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(wait_for_pending_shells(Duration::from_secs(5)).is_complete());

        let start = Instant::now();
        assert!(wait_for_pending_shells(Duration::from_secs(5)).is_complete());
        assert!(start.elapsed() < Duration::from_millis(50), "took {:?}", start.elapsed());
    }

    /// A shell that exited on its own releases its pending count without any thread.
    #[test]
    fn exited_shell_is_not_pending() {
        let _serial =
            super::super::child::HANDLE_COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(wait_for_pending_shells(Duration::from_secs(5)).is_complete());

        let mut child = spawn("cmd", &["/c", "exit 0"]);
        child.wait().unwrap();
        drop(new_reaper(&OwnedHandle::from(child)));
        assert!(wait_for_pending_shells(Duration::ZERO).is_complete());
    }

    /// If the reaper thread cannot be created, the dropping thread reaps the shell itself, and
    /// for a bounded time only.
    #[test]
    fn thread_creation_failure_falls_back_to_a_bounded_synchronous_reap() {
        use std::sync::atomic::Ordering;

        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                FAIL_THREAD_SPAWN.store(false, Ordering::SeqCst);
            }
        }

        let _serial =
            super::super::child::HANDLE_COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(wait_for_pending_shells(Duration::from_secs(5)).is_complete());

        let child = long_running();
        let observer = child.try_clone_handle();
        let reaper = new_reaper(&OwnedHandle::from(child));

        FAIL_THREAD_SPAWN.store(true, Ordering::SeqCst);
        let _reset = Reset;
        let start = Instant::now();
        drop(reaper);
        let elapsed = start.elapsed();

        assert!(elapsed >= FALLBACK_GRACE, "gave the shell no grace: {elapsed:?}");
        assert!(
            elapsed < FALLBACK_GRACE + FALLBACK_CONFIRM + Duration::from_millis(500),
            "blocked {elapsed:?}"
        );
        assert!(matches!(wait(&observer, Duration::ZERO), Wait::Exited), "shell left alive");
        assert_eq!(exit_code(&observer), Some(FORCED_EXIT_CODE));
        assert!(wait_for_pending_shells(Duration::ZERO).is_complete());
    }

    /// A handle that may wait but not terminate is reported as a failed termination, and the
    /// shell is left alone.
    #[test]
    fn termination_failure_is_reported() {
        use std::os::windows::io::FromRawHandle;
        use std::ptr;

        use windows_sys::Win32::Foundation::DuplicateHandle;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        const SYNCHRONIZE: u32 = 0x0010_0000;
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

        let _serial =
            super::super::child::HANDLE_COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut child = long_running();
        let full = child.try_clone_handle();

        let mut limited: HANDLE = ptr::null_mut();
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                full.as_raw_handle() as HANDLE,
                GetCurrentProcess(),
                &mut limited,
                SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                0,
            )
        };
        assert_ne!(ok, 0);
        let limited = unsafe { OwnedHandle::from_raw_handle(limited as _) };

        let outcome = reap(limited, Duration::from_millis(100), Duration::from_millis(300));
        assert_eq!(outcome, Outcome::TerminateFailed);
        assert!(
            matches!(wait(&full, Duration::ZERO), Wait::TimedOut),
            "the shell must be left alone"
        );

        child.kill().unwrap();
        child.wait().unwrap();
    }

    trait TryCloneHandle {
        fn try_clone_handle(&self) -> OwnedHandle;
    }
    impl TryCloneHandle for Child {
        fn try_clone_handle(&self) -> OwnedHandle {
            use std::os::windows::io::AsHandle;
            self.as_handle().try_clone_to_owned().unwrap()
        }
    }
}
