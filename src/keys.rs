//! Key bindings set from the proxy mod loader's manager (`swml_describe` / `swml_set`). Names are
//! imgui's, as the manager records them (`F10`, `Z`, `Alpha1`), with `Ctrl+` / `Shift+` / `Alt+`
//! in front; read with `GetAsyncKeyState`.

use std::ffi::{c_char, CStr};
use std::sync::atomic::{AtomicU32, Ordering};

pub struct Bind {
    default: &'static str,
    /// `UNSET` until set or first read; 0 = no key; else the virtual key and the modifier bits.
    cur: AtomicU32,
}

const UNSET: u32 = u32::MAX;
const MODS: [(u32, &str, i32); 3] = [(1 << 16, "Ctrl", 0x11), (1 << 17, "Shift", 0x10), (1 << 18, "Alt", 0x12)];

/// Virtual-key code of an imgui key name.
fn vk(name: &str) -> Option<u32> {
    let one = |p: &str, base: u32, n: u32| name.strip_prefix(p)?.parse::<u32>().ok().filter(|&i| i < n).map(|i| base + i);
    if let [c @ b'A'..=b'Z'] = name.as_bytes() {
        return Some(*c as u32);
    }
    if let Some(f) = name.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()).filter(|n| (1..=24).contains(n)) {
        return Some(0x6f + f);
    }
    if let Some(v) = one("Alpha", 0x30, 10).or_else(|| one("Keypad", 0x60, 10)) {
        return Some(v);
    }
    Some(match name {
        "Tab" => 0x09,
        "LeftArrow" => 0x25,
        "UpArrow" => 0x26,
        "RightArrow" => 0x27,
        "DownArrow" => 0x28,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "Home" => 0x24,
        "End" => 0x23,
        "Insert" => 0x2d,
        "Delete" => 0x2e,
        "Backspace" => 0x08,
        "Space" => 0x20,
        "Enter" | "KeypadEnter" => 0x0d,
        "Escape" => 0x1b,
        "LeftShift" => 0xa0,
        "RightShift" => 0xa1,
        "LeftCtrl" => 0xa2,
        "RightCtrl" => 0xa3,
        "LeftAlt" => 0xa4,
        "RightAlt" => 0xa5,
        "Menu" => 0x5d,
        "KeypadMultiply" => 0x6a,
        "KeypadAdd" => 0x6b,
        "KeypadSubtract" => 0x6d,
        "KeypadDecimal" => 0x6e,
        "KeypadDivide" => 0x6f,
        "Semicolon" => 0xba,
        "Equal" => 0xbb,
        "Comma" => 0xbc,
        "Minus" => 0xbd,
        "Period" => 0xbe,
        "Slash" => 0xbf,
        "GraveAccent" => 0xc0,
        "LeftBracket" => 0xdb,
        "Backslash" => 0xdc,
        "RightBracket" => 0xdd,
        "Apostrophe" => 0xde,
        "CapsLock" => 0x14,
        "ScrollLock" => 0x91,
        "NumLock" => 0x90,
        "PrintScreen" => 0x2c,
        "Pause" => 0x13,
        _ => return None,
    })
}

/// `Ctrl+F10` -> its code. None: a name this plugin does not know.
fn parse(s: &str) -> Option<u32> {
    if s.is_empty() || s == "None" {
        return Some(0);
    }
    let mut code = 0;
    for part in s.split('+').map(str::trim) {
        code |= match MODS.iter().find(|m| m.1 == part) {
            Some(m) => m.0,
            None => vk(part)?,
        };
    }
    Some(code)
}

impl Bind {
    pub const fn new(default: &'static str) -> Bind {
        Bind { default, cur: AtomicU32::new(UNSET) }
    }

    fn code(&self) -> u32 {
        match self.cur.load(Ordering::Relaxed) {
            UNSET => parse(self.default).unwrap_or(0),
            c => c,
        }
    }

    /// The key and its modifiers held now; None when `GetAsyncKeyState` is unavailable.
    pub fn down(&self) -> Option<bool> {
        let code = self.code();
        let key = code & 0xffff;
        if key == 0 {
            return Some(false);
        }
        let mut down = crate::flir::async_key_down(key as i32)?;
        for (bit, _, vk) in MODS {
            if code & bit != 0 {
                down &= crate::flir::async_key_down(vk)?;
            }
        }
        Some(down)
    }
}

pub static FLIR_VIEW: Bind = Bind::new("F10");
pub static DRAW_DUMP: Bind = Bind::new("F8");

fn list() -> [(&'static str, &'static Bind); 2] {
    [("flir_view", &FLIR_VIEW), ("draw_dump", &DRAW_DUMP)]
}

/// Both keys work in debug builds only, so a release build lists none.
#[no_mangle]
pub extern "C" fn swml_describe() -> *const c_char {
    if cfg!(debug_assertions) {
        c"key flir_view F10 FLIR main view (debug build) | Main view as the FLIR sees it.\nkey draw_dump F8 FLIR draw dump (debug build) | Log one frame's FLIR draws (FLIR view on).\n".as_ptr()
    } else {
        c"".as_ptr()
    }
}

/// # Safety
/// NUL-terminated strings (or null: ignored), from the mod loader.
#[no_mangle]
pub unsafe extern "C" fn swml_set(id: *const c_char, value: *const c_char) {
    if id.is_null() || value.is_null() {
        return;
    }
    let (id, value) = (CStr::from_ptr(id).to_string_lossy(), CStr::from_ptr(value).to_string_lossy());
    if let (Some((_, b)), Some(c)) = (list().into_iter().find(|(n, _)| *n == id), parse(&value)) {
        b.cur.store(c, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_to_virtual_keys() {
        assert_eq!(parse("F10"), Some(0x79));
        assert_eq!(parse("F8"), Some(0x77));
        assert_eq!(parse("Ctrl+Shift+Z"), Some((1 << 16) | (1 << 17) | 0x5a));
        assert_eq!(parse("Alpha1"), Some(0x31));
        assert_eq!(parse("Keypad9"), Some(0x69));
        assert_eq!(parse("None"), Some(0));
        assert_eq!(parse("F25"), None);
        assert_eq!(parse("Bogus"), None);
    }
}
