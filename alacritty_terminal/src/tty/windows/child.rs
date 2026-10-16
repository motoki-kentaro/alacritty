use std::ffi::c_void;
use std::io::Error;
use std::mem::ManuallyDrop;
use std::num::NonZeroU32;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::process::ExitStatus;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use log::warn;
use polling::os::iocp::{CompletionPacket, PollerIocpExt};
use polling::{Event, Poller};

use windows_sys::Win32::Foundation::{BOOLEAN, FALSE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessId, INFINITE, RegisterWaitForSingleObject, UnregisterWaitEx,
    WT_EXECUTEINWAITTHREAD, WT_EXECUTEONLYONCE,
};

use crate::tty::ChildEvent;

/// Process-wide handle counts are only meaningful if the tests measuring them do not run
/// concurrently, so they all hold this lock.
#[cfg(test)]
pub(super) static HANDLE_COUNT_LOCK: Mutex<()> = Mutex::new(());

struct Interest {
    poller: Arc<Poller>,
    event: Event,
}

/// Context shared with the wait callback.
///
/// It is owned by [`ChildExitWatcher`], which frees it only after the wait has been unregistered
/// and every in-flight callback has returned. The callback only ever borrows it.
struct ChildExitSender {
    sender: mpsc::Sender<ChildEvent>,
    interest: Arc<Mutex<Option<Interest>>>,
    child_handle: HANDLE,
}

/// WinAPI callback to run when child process exits.
extern "system" fn child_exit_callback(ctx: *mut c_void, timed_out: BOOLEAN) {
    if timed_out != 0 {
        return;
    }

    // SAFETY: The context outlives the callback, since `ChildExitWatcher::drop` blocks in
    // `UnregisterWaitEx` until all running callbacks have completed before freeing it.
    let event_tx = unsafe { &*(ctx as *const ChildExitSender) };

    let mut exit_code = 0_u32;
    let status = unsafe { GetExitCodeProcess(event_tx.child_handle, &mut exit_code) };
    let exit_status = if status == FALSE { None } else { Some(ExitStatus::from_raw(exit_code)) };
    event_tx.sender.send(ChildEvent::Exited(exit_status)).ok();

    let interest = event_tx.interest.lock().unwrap();
    if let Some(interest) = interest.as_ref() {
        interest.poller.post(CompletionPacket::new(interest.event)).ok();
    }
}

pub struct ChildExitWatcher {
    wait_handle: AtomicPtr<c_void>,
    /// Leaked `Box<ChildExitSender>`, reclaimed in `drop` once the wait is unregistered.
    context: AtomicPtr<ChildExitSender>,
    event_rx: mpsc::Receiver<ChildEvent>,
    interest: Arc<Mutex<Option<Interest>>>,
    /// Closed manually in `drop` once the wait is unregistered; leaked if unregistering fails.
    child_handle: ManuallyDrop<OwnedHandle>,
    pid: Option<NonZeroU32>,
}

impl ChildExitWatcher {
    /// Watch for the exit of `child_handle`.
    ///
    /// The watcher takes ownership of the process handle and closes it when dropped (or right
    /// away if registering the wait fails).
    pub fn new(child_handle: OwnedHandle) -> Result<ChildExitWatcher, Error> {
        let (event_tx, event_rx) = mpsc::channel();
        let raw_handle = child_handle.as_raw_handle() as HANDLE;

        let mut wait_handle: HANDLE = ptr::null_mut();
        let interest = Arc::new(Mutex::new(None));
        let context = Box::into_raw(Box::new(ChildExitSender {
            sender: event_tx,
            interest: interest.clone(),
            child_handle: raw_handle,
        }));

        let success = unsafe {
            RegisterWaitForSingleObject(
                &mut wait_handle,
                raw_handle,
                Some(child_exit_callback),
                context.cast(),
                INFINITE,
                WT_EXECUTEINWAITTHREAD | WT_EXECUTEONLYONCE,
            )
        };

        if success == 0 {
            let err = Error::last_os_error();
            // The wait was not registered, so nothing else can reference the context.
            drop(unsafe { Box::from_raw(context) });
            Err(err)
        } else {
            let pid = unsafe { NonZeroU32::new(GetProcessId(raw_handle)) };
            Ok(ChildExitWatcher {
                event_rx,
                interest,
                pid,
                child_handle: ManuallyDrop::new(child_handle),
                context: AtomicPtr::from(context),
                wait_handle: AtomicPtr::from(wait_handle),
            })
        }
    }

    pub fn event_rx(&self) -> &mpsc::Receiver<ChildEvent> {
        &self.event_rx
    }

    pub fn register(&self, poller: &Arc<Poller>, event: Event) {
        *self.interest.lock().unwrap() = Some(Interest { poller: poller.clone(), event });
    }

    pub fn deregister(&self) {
        *self.interest.lock().unwrap() = None;
    }

    /// Retrieve the process handle of the underlying child process.
    ///
    /// This function does **not** pass ownership of the raw handle to you,
    /// and the handle is only guaranteed to be valid while the hosted application
    /// has not yet been destroyed.
    ///
    /// If you terminate the process using this handle, the terminal will get a
    /// timeout error, and the child watcher will emit an `Exited` event.
    pub fn raw_handle(&self) -> HANDLE {
        self.child_handle.as_raw_handle() as HANDLE
    }

    /// Retrieve the Process ID associated to the underlying child process.
    pub fn pid(&self) -> Option<NonZeroU32> {
        self.pid
    }
}

impl Drop for ChildExitWatcher {
    fn drop(&mut self) {
        // Unregister the wait and block until any running callback has returned. The callback
        // only sends on a channel and posts to the poller, so this returns promptly. It must not
        // be called from within the callback itself.
        let wait_handle = self.wait_handle.load(Ordering::Relaxed) as HANDLE;
        let unregistered = unsafe { UnregisterWaitEx(wait_handle, INVALID_HANDLE_VALUE) };

        if unregistered == 0 {
            // A callback might still be using the context and the process handle, so leaking
            // them is the only safe option.
            warn!("Failed to unregister child exit wait: {}", Error::last_os_error());
            return;
        }

        // SAFETY: The wait is unregistered and no callback is running, so the context and the
        // process handle are no longer referenced by anyone else, and are released only once.
        unsafe {
            drop(Box::from_raw(self.context.load(Ordering::Relaxed)));
            ManuallyDrop::drop(&mut self.child_handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::windows::io::AsHandle;
    use std::process::{Child, Command};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

    use super::super::PTY_CHILD_EVENT_TOKEN;
    use super::*;

    fn watch(child: &Child) -> ChildExitWatcher {
        let handle = child.as_handle().try_clone_to_owned().unwrap();
        ChildExitWatcher::new(handle).unwrap()
    }

    fn handle_count() -> u32 {
        let mut count = 0;
        assert_ne!(unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) }, 0);
        count
    }

    fn spawn_sleeper() -> Child {
        Command::new("cmd.exe")
            .args(["/c", "pause"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    }

    #[test]
    pub fn event_is_emitted_when_child_exits() {
        let _serial = HANDLE_COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        const WAIT_TIMEOUT: Duration = Duration::from_millis(200);

        let poller = Arc::new(Poller::new().unwrap());

        let mut child = Command::new("cmd.exe").spawn().unwrap();
        let child_exit_watcher = watch(&child);
        child_exit_watcher.register(&poller, Event::readable(PTY_CHILD_EVENT_TOKEN));

        child.kill().unwrap();
        child.wait().unwrap();

        // Poll for the event or fail with timeout if nothing has been sent.
        let mut events = polling::Events::new();
        poller.wait(&mut events, Some(WAIT_TIMEOUT)).unwrap();
        assert_eq!(events.iter().next().unwrap().key, PTY_CHILD_EVENT_TOKEN);
        // Verify that at least one `ChildEvent::Exited` was received.
        let expected_status = ExitStatus::from_raw(1);
        assert_eq!(
            child_exit_watcher.event_rx().try_recv(),
            Ok(ChildEvent::Exited(Some(expected_status)))
        );
        // Exactly one notification is emitted.
        assert!(child_exit_watcher.event_rx().try_recv().is_err());
    }

    /// Dropping a watcher whose wait never fired must not leak the process handle, and must
    /// not block.
    #[test]
    fn drop_before_exit_releases_handle_without_blocking() {
        let _serial = HANDLE_COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut child = spawn_sleeper();
        let _ = watch(&child); // Warm up thread pool.
        let before = handle_count();

        let start = Instant::now();
        for _ in 0..100 {
            drop(watch(&child));
        }
        let elapsed = start.elapsed();

        assert_eq!(handle_count(), before, "process handle leaked");
        assert!(elapsed < Duration::from_secs(5), "drop blocked for {elapsed:?}");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// Dropping after the callback has run must release everything, too.
    #[test]
    fn drop_after_exit_releases_handle() {
        let _serial = HANDLE_COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut child = Command::new("cmd.exe").args(["/c", "exit 0"]).spawn().unwrap();
        child.wait().unwrap();
        let _ = watch(&child);
        let before = handle_count();

        for _ in 0..100 {
            let watcher = watch(&child);
            // The process is gone, so the callback fires (almost) immediately.
            let event = watcher.event_rx().recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(event, ChildEvent::Exited(Some(ExitStatus::from_raw(0))));
            drop(watcher);
        }

        assert_eq!(handle_count(), before, "process handle leaked");
    }

    /// Race the callback against drop, with exit and drop happening at about the same time.
    #[test]
    fn drop_races_with_exit() {
        let _serial = HANDLE_COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let before;
        {
            let mut child = Command::new("cmd.exe").args(["/c", "exit 0"]).spawn().unwrap();
            child.wait().unwrap();
            let _ = watch(&child);
            before = handle_count();
        }

        let start = Instant::now();
        for i in 0..200 {
            let mut child = spawn_sleeper();
            let watcher = watch(&child);
            let killer = std::thread::spawn(move || {
                child.kill().unwrap();
                child.wait().unwrap();
            });
            // Vary the interleaving.
            std::thread::sleep(Duration::from_micros((i % 20) * 50));
            drop(watcher);
            killer.join().unwrap();
        }
        assert!(start.elapsed() < Duration::from_secs(30), "drop/exit race hung");
        // The `Child` handles are gone; only the process handle of the leaked watchers could
        // remain.
        assert!(handle_count() <= before + 2, "process handle leaked");
    }

    /// `register` / `deregister` while a callback may be posting must not deadlock the drop.
    #[test]
    fn drop_with_registered_poller() {
        let _serial = HANDLE_COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let poller = Arc::new(Poller::new().unwrap());
        let mut child = spawn_sleeper();
        let watcher = watch(&child);
        watcher.register(&poller, Event::readable(PTY_CHILD_EVENT_TOKEN));
        child.kill().unwrap();
        child.wait().unwrap();
        drop(watcher);
    }
}
