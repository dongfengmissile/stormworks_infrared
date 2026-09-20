//! Raw process memory helpers: resolve a game function by RVA, verify a
//! function still matches its recorded prologue, and read primitives.

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use windows_sys::Win32::Foundation::HINSTANCE;

use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;

use crate::state::{MODULE_BASE, SELF_MODULE};

pub(crate) unsafe fn game_fn<T: Copy>(rva: usize) -> T {
    let address = MODULE_BASE.load(Ordering::SeqCst).wrapping_add(rva);
    std::mem::transmute_copy(&address)
}

pub(crate) fn bytes_match(address: usize, expected: &[u8]) -> bool {
    unsafe {
        let actual = std::slice::from_raw_parts(address as *const u8, expected.len());
        actual == expected
    }
}

pub(crate) fn self_module_path() -> Option<PathBuf> {
    let module = SELF_MODULE.load(Ordering::SeqCst);
    if module == 0 {
        return None;
    }
    let mut capacity = 512usize;
    loop {
        let mut buffer = vec![0u16; capacity];
        let len =
            unsafe { GetModuleFileNameW(module as HINSTANCE, buffer.as_mut_ptr(), capacity as u32) };
        if len == 0 {
            return None;
        }
        if (len as usize) < capacity - 1 {
            return Some(PathBuf::from(String::from_utf16_lossy(
                &buffer[..len as usize],
            )));
        }
        capacity *= 2;
    }
}

// ---------------------------------------------------------------------------
// Lua ABI (Stormworks' Lua 5.3, value = 16 bytes: value + u32 tag at +8)
// ---------------------------------------------------------------------------

pub(crate) unsafe fn read_usize_at(addr: usize) -> usize {
    *(addr as *const usize)
}
pub(crate) unsafe fn read_u32_at(addr: usize) -> u32 {
    *(addr as *const u32)
}
/// Best-effort check that `len` bytes at `address` can be read, so a hook can
/// refuse to touch a pointer it did not get from a register argument.
pub(crate) fn readable(address: usize, len: usize) -> bool {
    use core::ffi::c_void;
    if address < 0x10000 {
        return false;
    }
    extern "system" {
        fn IsBadReadPtr(lp: *const c_void, ucb: usize) -> i32;
    }
    unsafe { IsBadReadPtr(address as *const c_void, len) == 0 }
}

pub(crate) unsafe fn read_f32_at(addr: usize) -> f32 {
    *(addr as *const f32)
}
pub(crate) unsafe fn read_f64_at(addr: usize) -> f64 {
    *(addr as *const f64)
}
