//! Crash diagnostics for the verbose build.
//!
//! Two pieces, both nearly free:
//!
//!   * [`set`] - a stage breadcrumb. Every phase that can still be running when
//!     the game faults stores one `AtomicU32`, so a fault line says where we
//!     were (which phase of the block sim, the mesh map, a FLIR draw, ...).
//!   * [`install`] - a vectored exception handler (last in the chain, i.e.
//!     `AddVectoredExceptionHandler(0, ..)`) that writes ONE line per distinct
//!     fault address to the bootstrap log for access violations, illegal /
//!     privileged instructions, stack overflow, fast fail, heap corruption,
//!     integer divide-by-zero, the first 4 distinct C++ exceptions, and so on.
//!     It then returns `EXCEPTION_CONTINUE_SEARCH`. It never swallows an
//!     exception and never changes the faulting thread's state.
//!
//! The handler is written to be safe *inside* a fault: no allocation, no locks,
//! no Rust formatting - a fixed stack buffer plus direct `WriteFile` on a log
//! handle opened at install time (a handle opened here would be a lock, a
//! heap allocation and a file open on an exhausted stack).

#[cfg_attr(not(feature = "logging"), allow(unused_imports))]
use std::sync::atomic::{AtomicU32, Ordering};

/// Breadcrumb of the phase that is currently running, best effort (it is one
/// process-wide word, so with the render thread and the game thread both busy
/// it reports whichever phase stored last).
pub(crate) static STAGE: AtomicU32 = AtomicU32::new(stage::IDLE);

pub(crate) mod stage {
    pub(crate) const IDLE: u32 = 0;
    pub(crate) const BLOCKTEMP_TICK: u32 = 1;
    pub(crate) const BLOCKTEMP_ENUMERATE: u32 = 2;
    pub(crate) const BLOCKTEMP_COLLECT_CELLS: u32 = 3;
    pub(crate) const BLOCKTEMP_PROBE: u32 = 4;
    pub(crate) const BLOCKTEMP_PUBLISH: u32 = 5;
    pub(crate) const FLIR_MESH_MAP: u32 = 6;
    pub(crate) const FLIR_BLOCK_DRAW: u32 = 7;
    pub(crate) const FLIR_COMPOSITE: u32 = 8;
    pub(crate) const HEAT_TEMPERATURE_AT: u32 = 9;
    pub(crate) const FLIR_TONEMAP: u32 = 10;
    pub(crate) const FLIR_GL_DRAW_ELEMENTS: u32 = 11;
    pub(crate) const FLIR_GL_UNIFORM_MATRIX: u32 = 12;
    pub(crate) const FLIR_GL_BIND_FRAMEBUFFER: u32 = 13;
}

/// Name of a stage code, for the fault line. `&'static str`, so the handler
/// needs no allocation.
pub(crate) fn stage_name(code: u32) -> &'static str {
    match code {
        stage::BLOCKTEMP_TICK => "blocktemp::tick",
        stage::BLOCKTEMP_ENUMERATE => "blocktemp enumerate",
        stage::BLOCKTEMP_COLLECT_CELLS => "blocktemp collect_cells",
        stage::BLOCKTEMP_PROBE => "blocktemp probe",
        stage::BLOCKTEMP_PUBLISH => "blocktemp publish",
        stage::FLIR_MESH_MAP => "flir mesh-map build",
        stage::FLIR_BLOCK_DRAW => "flir block draw",
        stage::FLIR_COMPOSITE => "flir composite",
        stage::HEAT_TEMPERATURE_AT => "heat::temperature_at",
        stage::FLIR_TONEMAP => "flir tonemap hook",
        stage::FLIR_GL_DRAW_ELEMENTS => "glDrawElements thunk",
        stage::FLIR_GL_UNIFORM_MATRIX => "glUniformMatrix4fv thunk",
        stage::FLIR_GL_BIND_FRAMEBUFFER => "glBindFramebuffer thunk",
        _ => "idle",
    }
}

/// Record the phase that is about to run (compiled out of the player build).
#[inline]
pub(crate) fn set(code: u32) {
    #[cfg(feature = "logging")]
    STAGE.store(code, Ordering::Relaxed);
    #[cfg(not(feature = "logging"))]
    let _ = code;
}

/// Verbose builds: one lock-free line when the process exits through
/// ExitProcess (safe under the loader lock; other threads are already dead).
pub(crate) fn note_detach(reserved_non_null: bool) {
    #[cfg(feature = "logging")]
    veil::note_detach(reserved_non_null);
    #[cfg(not(feature = "logging"))]
    let _ = reserved_non_null;
}

/// Set the breadcrumb for the lifetime of the guard, then fall back to idle.
/// Use it where a phase has several returns and a panic must not leave the
/// breadcrumb claiming a phase that is over.
pub(crate) struct StageGuard(());

pub(crate) fn enter(code: u32) -> StageGuard {
    set(code);
    StageGuard(())
}

impl Drop for StageGuard {
    fn drop(&mut self) {
        set(stage::IDLE);
    }
}

// ---------------------------------------------------------------------------
// Vectored exception handler (verbose builds only)
// ---------------------------------------------------------------------------

#[cfg(feature = "logging")]
mod veil {
    use super::{stage_name, STAGE};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, EXCEPTION_ILLEGAL_INSTRUCTION,
        EXCEPTION_STACK_OVERFLOW, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, WriteFile, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, RtlCaptureStackBackTrace, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    use windows_sys::Win32::System::Memory::{VirtualQuery, MEM_IMAGE, MEMORY_BASIC_INFORMATION};
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;

    use crate::state::{MODULE_BASE, SELF_MODULE};

    // Exception codes the standard headers do not all name. `0xC000001D` is
    // EXCEPTION_ILLEGAL_INSTRUCTION and `0xC00000FD` is EXCEPTION_STACK_OVERFLOW.
    const STATUS_STACK_BUFFER_OVERRUN: u32 = 0xC0000409; // fast fail
    const STATUS_HEAP_CORRUPTION: u32 = 0xC0000374;
    const STATUS_INTEGER_DIVIDE_BY_ZERO: u32 = 0xC0000094;
    const STATUS_PRIVILEGED_INSTRUCTION: u32 = 0xC0000096;
    const STATUS_STACK_OVERFLOW: u32 = 0xC00000FD;
    const STATUS_CPP_EXCEPTION: u32 = 0xE06D7363;

    /// Append-only handle on the bootstrap log, opened once at install time so
    /// the handler itself only has to `WriteFile`.
    static LOG_HANDLE: AtomicUsize = AtomicUsize::new(0);
    /// Re-entrancy guard: a fault inside the handler (a guarded page, a bad
    /// stack) returns immediately instead of recursing.
    static IN_HANDLER: AtomicBool = AtomicBool::new(false);
    /// Fault addresses already reported, most recent last (one line each).
    static SEEN: [AtomicUsize; 8] = [const { AtomicUsize::new(0) }; 8];
    static SEEN_NEXT: AtomicUsize = AtomicUsize::new(0);
    /// The first 4 distinct C++ exception addresses (tracked separately so a
    /// normal fault at the same address cannot hide one, and vice versa).
    static CPP_SEEN: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];
    static CPP_NEXT: AtomicUsize = AtomicUsize::new(0);

    pub(super) fn install(log_path: &std::path::Path) {
        use std::os::windows::ffi::OsStrExt;
        let mut wide: Vec<u16> = log_path.as_os_str().encode_wide().collect();
        wide.push(0);
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_APPEND_DATA,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return;
        }
        LOG_HANDLE.store(handle as usize, Ordering::SeqCst);
        unsafe {
            // first = 0: run after the game's own handlers, so this only sees
            // what nobody else handled.
            let _ = AddVectoredExceptionHandler(0, Some(handler));
            hook_terminate();
        }
    }

    /// Little fixed-capacity ASCII line builder: no allocation, no formatting
    /// machinery that could take a lock or touch the heap.
    struct Line {
        bytes: [u8; 1024],
        len: usize,
    }

    impl Line {
        fn new() -> Line {
            Line {
                bytes: [0; 1024],
                len: 0,
            }
        }
        fn str(&mut self, text: &str) {
            let bytes = text.as_bytes();
            let room = self.bytes.len().saturating_sub(self.len);
            let take = bytes.len().min(room);
            self.bytes[self.len..self.len + take].copy_from_slice(&bytes[..take]);
            self.len += take;
        }
        fn hex(&mut self, value: usize) {
            self.str("0x");
            let mut started = false;
            for shift in (0..16).rev() {
                let digit = ((value >> (shift * 4)) & 0xf) as u8;
                if digit != 0 {
                    started = true;
                }
                if started || shift == 0 {
                    let c = if digit < 10 { b'0' + digit } else { b'a' + digit - 10 };
                    if self.len < self.bytes.len() {
                        self.bytes[self.len] = c;
                        self.len += 1;
                    }
                }
            }
        }
        fn dec(&mut self, value: u64) {
            let mut digits = [0u8; 20];
            let mut count = 0;
            let mut value = value;
            loop {
                digits[count] = b'0' + (value % 10) as u8;
                count += 1;
                value /= 10;
                if value == 0 {
                    break;
                }
            }
            while count > 0 {
                count -= 1;
                if self.len < self.bytes.len() {
                    self.bytes[self.len] = digits[count];
                    self.len += 1;
                }
            }
        }
        fn as_ptr(&self) -> *const u8 {
            self.bytes.as_ptr()
        }
        fn len(&self) -> u32 {
            self.len as u32
        }
    }

    /// `game exe` / `our DLL` / `other` plus the RVA inside that module.
    fn module(addr: usize, line: &mut Line) {
        let game = MODULE_BASE.load(Ordering::Relaxed);
        let ours = SELF_MODULE.load(Ordering::Relaxed);
        let mut info = MEMORY_BASIC_INFORMATION::default();
        let queried = unsafe {
            VirtualQuery(
                addr as *const core::ffi::c_void,
                &mut info,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        let image = queried != 0 && info.Type == MEM_IMAGE;
        let base = if image { info.AllocationBase as usize } else { 0 };
        let (name, module_base) = if base != 0 && base == game {
            ("game exe", game)
        } else if base != 0 && base == ours {
            ("our dll", ours)
        } else if base != 0 {
            ("other image", base)
        } else {
            ("no image", 0)
        };
        line.str(name);
        if module_base != 0 && module_base != game && module_base != ours {
            line.str("@");
            line.hex(module_base);
        }
        if module_base != 0 {
            line.str(" rva=");
            line.hex(addr.wrapping_sub(module_base));
        } else {
            line.str(" addr=");
            line.hex(addr);
        }
    }

    unsafe fn report(line: &mut Line) {
        let handle = LOG_HANDLE.load(Ordering::Relaxed) as HANDLE;
        if handle.is_null() {
            return;
        }
        let mut written = 0u32;
        WriteFile(
            handle,
            line.as_ptr(),
            line.len(),
            &mut written,
            std::ptr::null_mut(),
        );
        // One line per fault and a crash usually follows: get it out now rather
        // than trusting a later flush that may never happen.
        FlushFileBuffers(handle);
    }

    /// DLL_PROCESS_DETACH: ExitProcess ran (a clean or game-initiated exit).
    /// TerminateProcess / a kill never gets here, so a missing line means that.
    pub(super) fn note_detach(reserved_non_null: bool) {
        let mut line = Line::new();
        line.str(if reserved_non_null {
            "PROCESS EXIT: DLL_PROCESS_DETACH via ExitProcess, last stage="
        } else {
            "DLL unloaded (FreeLibrary), last stage="
        });
        line.str(stage_name(STAGE.load(Ordering::Relaxed)));
        line.str("\n");
        unsafe { report(&mut line) };
    }

    type NtTerminate = unsafe extern "system" fn(HANDLE, i32) -> i32;
    static NT_TERMINATE_ORIG: AtomicUsize = AtomicUsize::new(0);

    /// Every process exit, including TerminateProcess / abort paths that never
    /// reach DLL_PROCESS_DETACH: exit status plus the caller's stack.
    unsafe extern "system" fn nt_terminate_detour(handle: HANDLE, status: i32) -> i32 {
        // handle 0 = "other threads" (first half of ExitProcess), -1 = self.
        if handle as isize == -1 || handle.is_null() {
            let mut line = Line::new();
            line.str("PROCESS TERMINATE: NtTerminateProcess handle=");
            line.hex(handle as usize);
            line.str(" status=");
            line.hex(status as u32 as usize);
            line.str(" thread=");
            line.dec(GetCurrentThreadId() as u64);
            line.str(" stack:");
            let mut frames = [std::ptr::null_mut(); 10];
            let n = RtlCaptureStackBackTrace(1, 10, frames.as_mut_ptr(), std::ptr::null_mut());
            for f in &frames[..n as usize] {
                line.str(" ");
                module(*f as usize, &mut line);
            }
            line.str("\n");
            report(&mut line);
        }
        let orig: NtTerminate = std::mem::transmute(NT_TERMINATE_ORIG.load(Ordering::Relaxed));
        orig(handle, status)
    }

    unsafe fn hook_terminate() {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
        let ntdll = GetModuleHandleA(b"ntdll.dll\0".as_ptr());
        let Some(target) = GetProcAddress(ntdll, b"NtTerminateProcess\0".as_ptr()) else {
            return;
        };
        let target = target as *mut std::ffi::c_void;
        if let Ok(orig) = minhook::MinHook::create_hook(target, nt_terminate_detour as *mut _) {
            NT_TERMINATE_ORIG.store(orig as usize, Ordering::Relaxed);
            let _ = minhook::MinHook::enable_hook(target);
        }
    }

    pub(super) unsafe extern "system" fn handler(info: *mut EXCEPTION_POINTERS) -> i32 {
        if IN_HANDLER.swap(true, Ordering::Relaxed) {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        if info.is_null() || (*info).ExceptionRecord.is_null() {
            IN_HANDLER.store(false, Ordering::Relaxed);
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let record = (*info).ExceptionRecord;
        let code = (*record).ExceptionCode as u32;
        // Only the codes worth a line: anything else belongs to the game / the
        // debugger (or is a first-chance C++/SEH exception that is handled).
        let label = match code {
            c if c == EXCEPTION_ACCESS_VIOLATION as u32 => "access_violation",
            c if c == EXCEPTION_ILLEGAL_INSTRUCTION as u32 => "illegal_instruction",
            c if c == EXCEPTION_STACK_OVERFLOW as u32
                || c == STATUS_STACK_OVERFLOW => "stack_overflow",
            STATUS_STACK_BUFFER_OVERRUN => "fast_fail",
            STATUS_HEAP_CORRUPTION => "heap_corruption",
            STATUS_INTEGER_DIVIDE_BY_ZERO => "integer_divide_by_zero",
            STATUS_PRIVILEGED_INSTRUCTION => "privileged_instruction",
            STATUS_CPP_EXCEPTION => "cpp_exception",
            // Anything else error-severity (invalid CRT parameter 0xC0000417,
            // in-page error, ...) or a breakpoint: a quiet abort path.
            c if c >= 0xC000_0000 || c == 0x8000_0003 => "other",
            _ => {
                IN_HANDLER.store(false, Ordering::Relaxed);
                return EXCEPTION_CONTINUE_SEARCH;
            }
        };
        let cpp = code == STATUS_CPP_EXCEPTION;
        let address = (*record).ExceptionAddress as usize;
        // One line per distinct fault address: a fault loop would otherwise
        // fill the log with the same record. C++ exceptions get their own
        // first-4 ring, since they are thrown and usually handled.
        let seen = if cpp { &CPP_SEEN[..] } else { &SEEN[..] };
        if seen
            .iter()
            .any(|slot| slot.load(Ordering::Relaxed) == address && address != 0)
        {
            IN_HANDLER.store(false, Ordering::Relaxed);
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let next = if cpp { &CPP_NEXT } else { &SEEN_NEXT };
        let index = next.fetch_add(1, Ordering::Relaxed);
        if cpp {
            // Strictly the first 4 distinct C++ exception sites: after that,
            // stay silent instead of cycling (throws are common and handled).
            if index >= CPP_SEEN.len() {
                IN_HANDLER.store(false, Ordering::Relaxed);
                return EXCEPTION_CONTINUE_SEARCH;
            }
            CPP_SEEN[index].store(address, Ordering::Relaxed);
        } else {
            SEEN[index % SEEN.len()].store(address, Ordering::Relaxed);
        }

        let mut line = Line::new();
        line.str("FAULT ");
        line.str(label);
        line.str(" code=");
        line.hex(code as usize);
        line.str(" at=");
        line.hex(address);
        line.str(" (");
        module(address, &mut line);
        line.str(")");
        // C++ exceptions carry a compiler-specific argument block: report the
        // faulting site only, never trust (or decode) that payload.
        if code == EXCEPTION_ACCESS_VIOLATION as u32 && (*record).NumberParameters >= 2 {
            // ExceptionInformation[0]: 0 read, 1 write, 8 execute (DEP).
            let kind = (*record).ExceptionInformation[0];
            line.str(" access=");
            line.str(match kind {
                0 => "read",
                1 => "write",
                8 => "execute",
                _ => "flag",
            });
            line.str(" address=");
            line.hex((*record).ExceptionInformation[1]);
        }
        line.str(" thread=");
        line.dec(GetCurrentThreadId() as u64);
        let stage = STAGE.load(Ordering::Relaxed);
        line.str(" stage=");
        line.str(stage_name(stage));
        line.str(" (");
        line.dec(stage as u64);
        line.str(")\n");
        report(&mut line);
        IN_HANDLER.store(false, Ordering::Relaxed);
        // Never swallow: the game's / debugger's own handler still gets it.
        EXCEPTION_CONTINUE_SEARCH
    }
}

/// Install the fault handler on the bootstrap log. Verbose builds only: the
/// player build writes no diagnostics, so a fault line would be noise.
pub(crate) fn install(log_path: &std::path::Path) {
    #[cfg(feature = "logging")]
    veil::install(log_path);
    #[cfg(not(feature = "logging"))]
    let _ = log_path;
}
