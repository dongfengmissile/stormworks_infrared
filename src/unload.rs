//! Taking the plugin out of the running game for the proxy mod loader (`swml_ready` /
//! `swml_unload`), so it can be swapped for a new build without restarting.
//!
//! The loader unloads plugins newest first, so our hooks are the top ones on any function another
//! plugin hooks too: switching them off puts back what was there before us. Then no game thread may
//! still be in our code before the DLL is freed.
//!
//! Not undone: the `ir` functions already in the game's Lua states. A script calling one after the
//! unload crashes the game, so unload with no vehicles spawned (the manager says so).

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::{GetThreadContext, RtlLookupFunctionEntry, RtlVirtualUnwind, CONTEXT, CONTEXT_FULL_AMD64, UNW_FLAG_NHANDLER};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32};
use windows_sys::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, MEM_IMAGE, PAGE_GUARD, PAGE_NOACCESS};
use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread, THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME};

use crate::logging::log_line;
use crate::state::SELF_MODULE;

/// Set when unloading starts: background loops end, nothing new gets patched.
pub(crate) static STOP: AtomicBool = AtomicBool::new(false);
/// The bootstrap installed its hooks (or gave up).
pub(crate) static BOOTED: AtomicBool = AtomicBool::new(false);

/// This DLL's address range.
fn image() -> std::ops::Range<u64> {
    let base = SELF_MODULE.load(Ordering::SeqCst);
    if base == 0 {
        return 0..0;
    }
    // PE header: e_lfanew at 0x3C, SizeOfImage at 0x50 into the NT headers.
    let size = unsafe { ((base + *((base + 0x3C) as *const u32) as usize + 0x50) as *const u32).read_unaligned() };
    base as u64..base as u64 + size as u64
}

fn region(addr: u64) -> Option<MEMORY_BASIC_INFORMATION> {
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { zeroed() };
    let ok = unsafe { VirtualQuery(addr as *const c_void, &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) } != 0;
    (ok && info.State == MEM_COMMIT && info.Protect & (PAGE_GUARD | PAGE_NOACCESS) == 0).then_some(info)
}

#[repr(C, align(16))]
struct Ctx(CONTEXT);

/// Whether stopped thread `tid` runs, or will return, into `range` (walked by the modules' unwind
/// tables). Allocates nothing while the thread is stopped: it may hold the heap lock.
unsafe fn in_range(tid: u32, range: &std::ops::Range<u64>) -> bool {
    let h = OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION, 0, tid);
    if h.is_null() {
        return false;
    }
    let mut found = false;
    if SuspendThread(h) != u32::MAX {
        let mut c: Ctx = zeroed();
        c.0.ContextFlags = CONTEXT_FULL_AMD64;
        if GetThreadContext(h, &mut c.0) != 0 {
            let c = &mut c.0;
            for _ in 0..40 {
                if c.Rip == 0 {
                    break;
                }
                if range.contains(&c.Rip) {
                    found = true;
                    break;
                }
                // Code outside any module (Wine's Unix side, generated code): no tables to go on.
                if region(c.Rip).is_none_or(|r| r.Type != MEM_IMAGE) {
                    break;
                }
                let mut base = 0u64;
                let f = RtlLookupFunctionEntry(c.Rip, &mut base, null_mut());
                if f.is_null() {
                    // A leaf function: the return address is on top of the stack.
                    if region(c.Rsp).is_none() {
                        break;
                    }
                    c.Rip = *(c.Rsp as *const u64);
                    c.Rsp += 8;
                } else {
                    let (mut data, mut frame) = (null_mut(), 0u64);
                    RtlVirtualUnwind(UNW_FLAG_NHANDLER, base, c.Rip, f, c, &mut data, &mut frame, null_mut());
                }
            }
        }
        ResumeThread(h);
    }
    CloseHandle(h);
    found
}

fn other_threads() -> Vec<u32> {
    let (me, pid) = unsafe { (GetCurrentThreadId(), GetCurrentProcessId()) };
    let mut tids = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap == INVALID_HANDLE_VALUE {
            return tids;
        }
        let mut e: THREADENTRY32 = zeroed();
        e.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut ok = Thread32First(snap, &mut e) != 0;
        while ok {
            if e.th32OwnerProcessID == pid && e.th32ThreadID != me {
                tids.push(e.th32ThreadID);
            }
            ok = Thread32Next(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    tids
}

/// Waits until no other thread runs or returns into this DLL. False after `timeout`.
fn quiet(timeout: Duration) -> bool {
    let range = image();
    let until = Instant::now() + timeout;
    loop {
        match other_threads().into_iter().find(|&tid| unsafe { in_range(tid, &range) }) {
            None => return true,
            Some(tid) if Instant::now() >= until => {
                log_line(&format!("unload: thread {tid} is still in the plugin"));
                return false;
            }
            Some(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// Mod loader: hooks in, so the next plugin may load.
#[no_mangle]
pub extern "C" fn swml_ready() -> bool {
    BOOTED.load(Ordering::SeqCst)
}

/// Mod loader: out of the game, so the DLL can be freed. False: not safe to free (logged).
#[no_mangle]
pub extern "C" fn swml_unload() -> bool {
    if !BOOTED.load(Ordering::SeqCst) {
        log_line("unload refused: the plugin is still starting");
        return false;
    }
    STOP.store(true, Ordering::SeqCst);
    // The game's GL function pointers back first: they are not hooks, nothing else restores them.
    unsafe { crate::flir::restore_gl_slots() };
    // One MinHook per DLL; ours are the top hooks, so each target gets back what was there before
    // us. The trampolines stay allocated for threads still on their way through them.
    if let Err(e) = unsafe { minhook::MinHook::disable_all_hooks() } {
        log_line(&format!("unload: hooks not disabled ({e:?}); not unloaded"));
        return false;
    }
    crate::fault::uninstall();
    if !quiet(Duration::from_secs(5)) {
        return false;
    }
    log_line("unloaded");
    true
}
