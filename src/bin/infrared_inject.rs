//! infrared injector.
//!
//! Loads `StormworksInfrared.dll` into a running `stormworks64.exe` process
//! the standard way: `VirtualAllocEx` + `WriteProcessMemory` a UTF-16 DLL path
//! into the target, then `CreateRemoteThread` with `LoadLibraryW` as the thread
//! start routine.
//!
//! The plugin DLL performs its own bootstrap (hook install) from `DllMain` via
//! a helper thread, so no further RPC is needed after injection.
//!
//! Usage:
//!
//! ```text
//! infrared_inject.exe                     # find stormworks64.exe, inject the sibling DLL
//! infrared_inject.exe --pid 12345         # inject into a specific process
//! infrared_inject.exe --dll C:\path\StormworksInfrared.dll
//! ```

#[cfg(windows)]
mod imp {
    use std::{
        ffi::{c_void, OsStr},
        os::windows::ffi::OsStrExt,
        path::{Path, PathBuf},
        process::exit,
    };

    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
        System::{
            Diagnostics::Debug::WriteProcessMemory,
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
                TH32CS_SNAPPROCESS,
            },
            LibraryLoader::{GetModuleHandleW, GetProcAddress},
            Memory::{
                VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
            },
            Threading::{
                CreateRemoteThread, GetExitCodeThread, OpenProcess, WaitForSingleObject,
                PROCESS_CREATE_THREAD, PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION,
                PROCESS_VM_READ, PROCESS_VM_WRITE,
            },
        },
    };

    const DEFAULT_PROCESS_NAME: &str = "stormworks64.exe";
    const DEFAULT_DLL_NAME: &str = "StormworksInfrared.dll";
    const INJECTION_TIMEOUT_MS: u32 = 15_000;
    /// `LoadLibraryW` returns a 64-bit `HMODULE` truncated to the thread exit
    /// code, so only "nonzero" is a meaningful success signal.
    const LOAD_LIBRARY_FAILED_EXIT_CODE: u32 = 0;

    struct Options {
        pid: Option<u32>,
        process_name: String,
        dll: Option<PathBuf>,
    }

    fn parse_args() -> Options {
        let mut options = Options {
            pid: None,
            process_name: DEFAULT_PROCESS_NAME.to_string(),
            dll: None,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--pid" => {
                    let value = args.next().unwrap_or_else(|| die("--pid needs a value"));
                    options.pid = Some(value.parse().unwrap_or_else(|error| {
                        die(&format!("invalid --pid {value:?}: {error}"))
                    }));
                }
                "--process-name" => {
                    options.process_name = args
                        .next()
                        .unwrap_or_else(|| die("--process-name needs a value"));
                }
                "--dll" => {
                    let value = args.next().unwrap_or_else(|| die("--dll needs a value"));
                    options.dll = Some(PathBuf::from(value));
                }
                "--help" | "-h" => {
                    print_usage();
                    exit(0);
                }
                other => die(&format!("unknown argument {other:?} (see --help)")),
            }
        }
        options
    }

    fn print_usage() {
        println!(
            "usage: infrared_inject.exe [--pid <pid>] [--process-name <name>] [--dll <path>]"
        );
        println!("  default process name: {DEFAULT_PROCESS_NAME}");
        println!("  default dll: <game dir>\\{DEFAULT_DLL_NAME}");
    }

    fn die(message: &str) -> ! {
        eprintln!("infrared_inject: {message}");
        exit(1)
    }

    fn last_error_message(what: &str) -> String {
        let code = unsafe { GetLastError() };
        format!("{what} failed (GetLastError={code})")
    }

    fn to_wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn find_process_id(process_name: &str) -> Option<u32> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = None;
        unsafe {
            if Process32FirstW(snapshot, &mut entry) != 0 {
                loop {
                    let name = String::from_utf16_lossy(
                        &entry.szExeFile
                            [..entry.szExeFile.iter().position(|c| *c == 0).unwrap_or(0)],
                    );
                    if name.eq_ignore_ascii_case(process_name) {
                        found = Some(entry.th32ProcessID);
                        break;
                    }
                    if Process32NextW(snapshot, &mut entry) == 0 {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snapshot);
        }
        found
    }

    fn default_dll_path() -> Option<PathBuf> {
        let injector_exe = std::env::current_exe().ok()?;
        let game_dir = injector_exe.parent()?;
        Some(game_dir.join(DEFAULT_DLL_NAME))
    }

    pub fn run() -> Result<(), String> {
        let options = parse_args();
        let pid = match options.pid {
            Some(pid) => pid,
            None => find_process_id(&options.process_name).ok_or_else(|| {
                format!(
                    "could not find a running process named {:?}; start Stormworks first or pass --pid",
                    options.process_name
                )
            })?,
        };

        let dll_path = options
            .dll
            .or_else(default_dll_path)
            .ok_or_else(|| "could not determine the plugin DLL path; pass --dll")?;
        if !dll_path.is_file() {
            return Err(format!("plugin DLL not found: {}", dll_path.display()));
        }

        // Resolve the DLL path up front so injection never loads through a
        // relative path that depends on the target's working directory.
        let dll_path = absolute_path(&dll_path)?;

        println!("injecting {} into pid {pid}", dll_path.display());
        let dll_wide = to_wide(dll_path.as_os_str());
        let dll_bytes_len = dll_wide.len() * std::mem::size_of::<u16>();

        unsafe {
            let process = OpenProcess(
                PROCESS_CREATE_THREAD
                    | PROCESS_QUERY_INFORMATION
                    | PROCESS_VM_OPERATION
                    | PROCESS_VM_READ
                    | PROCESS_VM_WRITE,
                0,
                pid,
            );
            if process.is_null() {
                return Err(last_error_message("OpenProcess"));
            }

            let remote_path = VirtualAllocEx(
                process,
                std::ptr::null(),
                dll_bytes_len,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            );
            if remote_path.is_null() {
                let error = last_error_message("VirtualAllocEx");
                let _ = CloseHandle(process);
                return Err(error);
            }

            let mut written = 0usize;
            if WriteProcessMemory(
                process,
                remote_path,
                dll_wide.as_ptr().cast::<c_void>(),
                dll_bytes_len,
                &mut written,
            ) == 0
                || written != dll_bytes_len
            {
                let error = last_error_message("WriteProcessMemory");
                let _ = VirtualFreeEx(process, remote_path, 0, MEM_RELEASE);
                let _ = CloseHandle(process);
                return Err(error);
            }

            let kernel32 = to_wide(OsStr::new("kernel32.dll"));
            let module = GetModuleHandleW(kernel32.as_ptr());
            if module.is_null() {
                let error = last_error_message("GetModuleHandleW(kernel32)");
                let _ = VirtualFreeEx(process, remote_path, 0, MEM_RELEASE);
                let _ = CloseHandle(process);
                return Err(error);
            }
            let load_library = GetProcAddress(module, c"LoadLibraryW".as_ptr().cast());
            let Some(load_library) = load_library else {
                let _ = VirtualFreeEx(process, remote_path, 0, MEM_RELEASE);
                let _ = CloseHandle(process);
                return Err("LoadLibraryW not found in kernel32.dll".to_string());
            };

            let thread = CreateRemoteThread(
                process,
                std::ptr::null(),
                0,
                Some(std::mem::transmute::<
                    _,
                    unsafe extern "system" fn(*mut c_void) -> u32,
                >(load_library)),
                remote_path,
                0,
                std::ptr::null_mut(),
            );
            if thread.is_null() {
                let error = last_error_message("CreateRemoteThread");
                let _ = VirtualFreeEx(process, remote_path, 0, MEM_RELEASE);
                let _ = CloseHandle(process);
                return Err(error);
            }

            if WaitForSingleObject(thread, INJECTION_TIMEOUT_MS) != WAIT_OBJECT_0 {
                let error = format!(
                    "remote LoadLibraryW thread did not finish within {INJECTION_TIMEOUT_MS} ms"
                );
                let _ = CloseHandle(thread);
                let _ = VirtualFreeEx(process, remote_path, 0, MEM_RELEASE);
                let _ = CloseHandle(process);
                return Err(error);
            }

            let mut exit_code = 0u32;
            if GetExitCodeThread(thread, &mut exit_code) == 0 {
                let error = last_error_message("GetExitCodeThread");
                let _ = CloseHandle(thread);
                let _ = VirtualFreeEx(process, remote_path, 0, MEM_RELEASE);
                let _ = CloseHandle(process);
                return Err(error);
            }
            let _ = CloseHandle(thread);
            let _ = VirtualFreeEx(process, remote_path, 0, MEM_RELEASE);
            let _ = CloseHandle(process);

            if exit_code == LOAD_LIBRARY_FAILED_EXIT_CODE {
                let log_hint = dll_path
                    .parent()
                    .map(|dir| dir.join("logs").join("infrared_bootstrap.log"))
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "logs\\infrared_bootstrap.log".to_string());
                return Err(format!(
                    "remote LoadLibraryW returned 0; check {log_hint} for details"
                ));
            }
        }

        println!("injection succeeded; the plugin bootstraps itself from DllMain");
        Ok(())
    }

    fn absolute_path(path: &Path) -> Result<PathBuf, String> {
        std::fs::canonicalize(path)
            .map_err(|error| format!("resolving {}: {error}", path.display()))
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn run() -> Result<(), String> {
        Err("the injector is only available on Windows".to_string())
    }
}

fn main() {
    if let Err(error) = imp::run() {
        eprintln!("infrared_inject: {error}");
        std::process::exit(1);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::imp;

    #[test]
    fn injector_links() {
        let _ = imp;
    }
}
