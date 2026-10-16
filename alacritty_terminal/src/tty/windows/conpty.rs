use log::{info, warn};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{Error, Result};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::{mem, ptr};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, S_OK};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::core::{HRESULT, PWSTR};
use windows_sys::{s, w};

use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    STARTUPINFOW, UpdateProcThreadAttribute,
};

use crate::event::{OnResize, WindowSize};
use crate::tty::Options;
use crate::tty::windows::blocking::{UnblockedReader, UnblockedWriter};
use crate::tty::windows::child::ChildExitWatcher;
use crate::tty::windows::{Pty, cmdline, win32_string};

const PIPE_CAPACITY: usize = crate::event_loop::READ_BUFFER_SIZE;

/// Load the pseudoconsole API from conpty.dll if possible, otherwise use the
/// standard Windows API.
///
/// The conpty.dll from the Windows Terminal project
/// supports loading OpenConsole.exe, which offers many improvements and
/// bugfixes compared to the standard conpty that ships with Windows.
///
/// The conpty.dll and OpenConsole.exe files will be searched in PATH and in
/// the directory where Alacritty's executable is located.
type CreatePseudoConsoleFn =
    unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> HRESULT;
type ResizePseudoConsoleFn = unsafe extern "system" fn(HPCON, COORD) -> HRESULT;
type ClosePseudoConsoleFn = unsafe extern "system" fn(HPCON);

struct ConptyApi {
    create: CreatePseudoConsoleFn,
    resize: ResizePseudoConsoleFn,
    close: ClosePseudoConsoleFn,
}

impl ConptyApi {
    fn new() -> Self {
        match Self::load_conpty() {
            Some(conpty) => {
                info!("Using conpty.dll for pseudoconsole");
                conpty
            },
            None => {
                // Cannot load conpty.dll - use the standard Windows API.
                info!("Using Windows API for pseudoconsole");
                Self {
                    create: CreatePseudoConsole,
                    resize: ResizePseudoConsole,
                    close: ClosePseudoConsole,
                }
            },
        }
    }

    /// Try loading ConptyApi from conpty.dll library.
    fn load_conpty() -> Option<Self> {
        type LoadedFn = unsafe extern "system" fn() -> isize;
        unsafe {
            let hmodule = LoadLibraryW(w!("conpty.dll"));
            if hmodule.is_null() {
                return None;
            }
            let create_fn = GetProcAddress(hmodule, s!("CreatePseudoConsole"))?;
            let resize_fn = GetProcAddress(hmodule, s!("ResizePseudoConsole"))?;
            let close_fn = GetProcAddress(hmodule, s!("ClosePseudoConsole"))?;

            Some(Self {
                create: mem::transmute::<LoadedFn, CreatePseudoConsoleFn>(create_fn),
                resize: mem::transmute::<LoadedFn, ResizePseudoConsoleFn>(resize_fn),
                close: mem::transmute::<LoadedFn, ClosePseudoConsoleFn>(close_fn),
            })
        }
    }
}

/// RAII Pseudoconsole.
pub struct Conpty {
    pub handle: HPCON,
    api: ConptyApi,
}

impl Drop for Conpty {
    fn drop(&mut self) {
        // XXX: This will block until the conout pipe is drained. Will cause a deadlock if the
        // conout pipe has already been dropped by this point.
        //
        // See PR #3084 and https://docs.microsoft.com/en-us/windows/console/closepseudoconsole.
        unsafe { (self.api.close)(self.handle) }
    }
}

// The ConPTY handle can be sent between threads.
unsafe impl Send for Conpty {}

/// Deletes an initialized `PROC_THREAD_ATTRIBUTE_LIST` on drop.
///
/// Must only be created after `InitializeProcThreadAttributeList` succeeded, and must be dropped
/// before the buffer backing the list.
struct AttributeListGuard(LPPROC_THREAD_ATTRIBUTE_LIST);

impl Drop for AttributeListGuard {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.0) };
    }
}

pub fn new(config: &Options, window_size: WindowSize) -> Result<Pty> {
    let api = ConptyApi::new();
    let mut pty_handle: HPCON = 0;

    // Passing 0 as the size parameter allows the "system default" buffer
    // size to be used. There may be small performance and memory advantages
    // to be gained by tuning this in the future, but it's likely a reasonable
    // start point.
    let (conout, conout_pty_handle) = miow::pipe::anonymous(0)?;
    let (conin_pty_handle, conin) = miow::pipe::anonymous(0)?;

    // Create the Pseudo Console, using the pipes.
    let result = unsafe {
        (api.create)(
            window_size.into(),
            conin_pty_handle.as_raw_handle() as HANDLE,
            conout_pty_handle.as_raw_handle() as HANDLE,
            0,
            &mut pty_handle as *mut _,
        )
    };

    // The pseudoconsole holds its own references to its end of the pipes, so ours must be
    // closed to avoid leaking them.
    drop(conin_pty_handle);
    drop(conout_pty_handle);

    assert_eq!(result, S_OK);

    // The pseudoconsole is owned from here on, so that every early return below closes it.
    //
    // The drain threads are started before the `Conpty` guard is created: locals are dropped in
    // reverse order, so `conpty` is closed first while `conout` is still being read, which is
    // required to not deadlock in `ClosePseudoConsole` (same order as the fields of `Pty`).
    let conin = UnblockedWriter::new(conin, PIPE_CAPACITY);
    let conout = UnblockedReader::new(conout, PIPE_CAPACITY);
    let conpty = Conpty { handle: pty_handle as HPCON, api };

    let mut success;

    // Prepare child process startup info.

    let mut size: usize = 0;

    let mut startup_info_ex: STARTUPINFOEXW = unsafe { mem::zeroed() };

    startup_info_ex.StartupInfo.lpTitle = std::ptr::null_mut() as PWSTR;

    startup_info_ex.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;

    // Setting this flag but leaving all the handles as default (null) ensures the
    // PTY process does not inherit any handles from this Alacritty process.
    startup_info_ex.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;

    // Create the appropriately sized thread attribute list.
    unsafe {
        let failure =
            InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size as *mut usize) > 0;

        // This call was expected to return false.
        if failure {
            return Err(Error::last_os_error());
        }
    }

    let mut attr_list: Box<[u8]> = vec![0; size].into_boxed_slice();

    // Set startup info's attribute list & initialize it
    //
    // Lint failure is spurious; it's because winapi's definition of PROC_THREAD_ATTRIBUTE_LIST
    // implies it is one pointer in size (32 or 64 bits) but really this is just a dummy value.
    // Casting a *mut u8 (pointer to 8 bit type) might therefore not be aligned correctly in
    // the compiler's eyes.
    #[allow(clippy::cast_ptr_alignment)]
    {
        startup_info_ex.lpAttributeList = attr_list.as_mut_ptr() as _;
    }

    unsafe {
        success = InitializeProcThreadAttributeList(
            startup_info_ex.lpAttributeList,
            1,
            0,
            &mut size as *mut usize,
        ) > 0;

        if !success {
            return Err(Error::last_os_error());
        }
    }

    // The list is initialized from here on and must be deleted once it is no longer needed. This
    // is declared after `attr_list` (and `conpty`), so it runs before the buffer is freed.
    let _attr_list_guard = AttributeListGuard(startup_info_ex.lpAttributeList);

    // Set thread attribute list's Pseudo Console to the specified ConPTY.
    unsafe {
        success = UpdateProcThreadAttribute(
            startup_info_ex.lpAttributeList,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            conpty.handle as *mut std::ffi::c_void,
            mem::size_of::<HPCON>(),
            ptr::null_mut(),
            ptr::null_mut(),
        ) > 0;

        if !success {
            return Err(Error::last_os_error());
        }
    }

    // Prepare child process creation arguments.
    let cmdline = win32_string(&cmdline(config));
    let cwd = config.working_directory.as_ref().map(win32_string);
    let mut creation_flags = EXTENDED_STARTUPINFO_PRESENT;
    let custom_env_block = convert_custom_env(&config.env);
    let custom_env_block_pointer = match &custom_env_block {
        Some(custom_env_block) => {
            creation_flags |= CREATE_UNICODE_ENVIRONMENT;
            custom_env_block.as_ptr() as *mut std::ffi::c_void
        },
        None => ptr::null_mut(),
    };

    let mut proc_info: PROCESS_INFORMATION = unsafe { mem::zeroed() };
    unsafe {
        success = CreateProcessW(
            ptr::null(),
            cmdline.as_ptr() as PWSTR,
            ptr::null_mut(),
            ptr::null_mut(),
            false as i32,
            creation_flags,
            custom_env_block_pointer,
            cwd.as_ref().map_or_else(ptr::null, |s| s.as_ptr()),
            &mut startup_info_ex.StartupInfo as *mut STARTUPINFOW,
            &mut proc_info as *mut PROCESS_INFORMATION,
        ) > 0;

        if !success {
            return Err(Error::last_os_error());
        }

        // The primary thread handle is not needed, only the process handle is used.
        CloseHandle(proc_info.hThread);
    }

    // SAFETY: `hProcess` is a valid handle returned by `CreateProcessW`, owned by nobody else.
    let child_handle = unsafe { OwnedHandle::from_raw_handle(proc_info.hProcess as RawHandle) };

    let child_watcher = ChildExitWatcher::new(child_handle)?;

    Ok(Pty::new(conpty, conout, conin, child_watcher))
}

// Windows environment variables are case-insensitive, and the caller is responsible for
// deduplicating environment variables, so do that here while converting.
//
// https://learn.microsoft.com/en-us/previous-versions/troubleshoot/windows/win32/createprocess-cannot-eliminate-duplicate-variables#environment-variables
fn convert_custom_env(custom_env: &HashMap<String, String>) -> Option<Vec<u16>> {
    // Windows inherits parent's env when no `lpEnvironment` parameter is specified.
    if custom_env.is_empty() {
        return None;
    }

    let mut converted_block = Vec::new();
    let mut all_env_keys = HashSet::new();
    for (custom_key, custom_value) in custom_env {
        let custom_key_os = OsStr::new(custom_key);
        if all_env_keys.insert(custom_key_os.to_ascii_uppercase()) {
            add_windows_env_key_value_to_block(
                &mut converted_block,
                custom_key_os,
                OsStr::new(&custom_value),
            );
        } else {
            warn!(
                "Omitting environment variable pair with duplicate key: \
                 '{custom_key}={custom_value}'"
            );
        }
    }

    // Pull the current process environment after, to avoid overwriting the user provided one.
    for (inherited_key, inherited_value) in std::env::vars_os() {
        if all_env_keys.insert(inherited_key.to_ascii_uppercase()) {
            add_windows_env_key_value_to_block(
                &mut converted_block,
                &inherited_key,
                &inherited_value,
            );
        }
    }

    converted_block.push(0);
    Some(converted_block)
}

// According to the `lpEnvironment` parameter description:
// https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessa#parameters
//
// > An environment block consists of a null-terminated block of null-terminated strings. Each
// string is in the following form:
// >
// > name=value\0
fn add_windows_env_key_value_to_block(block: &mut Vec<u16>, key: &OsStr, value: &OsStr) {
    block.extend(key.encode_wide());
    block.push('=' as u16);
    block.extend(value.encode_wide());
    block.push(0);
}

impl OnResize for Conpty {
    fn on_resize(&mut self, window_size: WindowSize) {
        let result = unsafe { (self.api.resize)(self.handle, window_size.into()) };
        assert_eq!(result, S_OK);
    }
}

impl From<WindowSize> for COORD {
    fn from(window_size: WindowSize) -> Self {
        let lines = window_size.num_lines;
        let columns = window_size.num_cols;
        COORD { X: columns as i16, Y: lines as i16 }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

    use super::super::child::HANDLE_COUNT_LOCK;
    use super::*;
    use crate::tty::Shell;

    fn handle_count() -> u32 {
        let mut count = 0;
        assert_ne!(unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) }, 0);
        count
    }

    fn window_size() -> WindowSize {
        WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 16 }
    }

    fn options(program: &str) -> Options {
        Options {
            shell: Some(Shell::new(program.into(), Vec::new())),
            drain_on_exit: false,
            working_directory: None,
            env: HashMap::new(),
            escape_args: true,
        }
    }

    /// `CreateProcessW` failing after the pseudoconsole exists must release the pseudoconsole
    /// and both pipes, and must not block.
    #[test]
    fn failed_process_creation_releases_pseudoconsole() {
        let _serial = HANDLE_COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let bad = options("kegon-no-such-program-4f1c.exe");

        // Warm up lazily created process-wide state.
        assert!(new(&bad, window_size()).is_err());
        let before = handle_count();

        let start = Instant::now();
        for _ in 0..100 {
            assert!(new(&bad, window_size()).is_err());
        }
        let elapsed = start.elapsed();

        // Give the reader threads a moment to observe the closed pseudoconsole and exit.
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle_count() > before && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(handle_count() <= before, "handles leaked: {before} -> {}", handle_count());
        assert!(elapsed < Duration::from_secs(30), "failed creation blocked for {elapsed:?}");
    }

    /// The successful path stays leak free.
    #[test]
    fn successful_creation_and_drop_does_not_leak() {
        let _serial = HANDLE_COUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let good = options("cmd.exe");

        drop(new(&good, window_size()).unwrap());
        std::thread::sleep(Duration::from_millis(500));
        let before = handle_count();

        for _ in 0..30 {
            drop(new(&good, window_size()).unwrap());
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        while handle_count() > before && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(handle_count() <= before, "handles leaked: {before} -> {}", handle_count());
    }
}
