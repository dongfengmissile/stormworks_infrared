//! Thin wrappers over the Lua C API the game exposes to components.

use std::ffi::CString;

use crate::memory::{game_fn, read_usize_at};
use crate::offsets::{
    FIRST_UPVALUE_INDEX, RVA_ARG_SLOT, RVA_CREATE_TABLE, RVA_PUSH_STRING, RVA_RAWSETI,
};

#[repr(C)]
pub(crate) struct FnPair {
    pub(crate) name: *const u8,
    pub(crate) func: usize,
}

// Diagnostics: log the first temperature capture, and a throttled summary of
// the raw temperatures a scan actually sees (for calibrating the vis window).
pub(crate) unsafe fn lua_arg_slot(lua: usize, index: i32) -> *mut u8 {
    let f: unsafe extern "C" fn(usize, i32) -> *mut u8 = game_fn(RVA_ARG_SLOT);
    f(lua, index)
}

pub(crate) unsafe fn lua_arg_number(lua: usize, index: i32) -> Option<f64> {
    let slot = lua_arg_slot(lua, index);
    if slot.is_null() {
        return None;
    }
    match *(slot.add(8) as *const u32) {
        3 => Some(*(slot as *const f64)),
        0x13 => Some(*(slot as *const i64) as f64),
        _ => None,
    }
}

/// A boolean argument (Lua 5.3 tag 1) or a number treated as a flag.
///
/// `lua_arg_number` only accepts tags 3 and 0x13, so a Lua `true`/`false`
/// arrives here as `None` unless this is used. Any non-zero number counts as
/// true, so an older script can keep passing `1` / `0`.
pub(crate) unsafe fn lua_arg_bool(lua: usize, index: i32) -> Option<bool> {
    let slot = lua_arg_slot(lua, index);
    if slot.is_null() {
        return None;
    }
    match *(slot.add(8) as *const u32) {
        1 => Some(*(slot as *const u32) != 0),
        _ => lua_arg_number(lua, index).map(|value| value != 0.0),
    }
}

pub(crate) unsafe fn lua_stack_top(lua: usize) -> usize {
    *(lua as *const usize).add(2)
}

pub(crate) unsafe fn lua_push_number(lua: usize, value: f64) {
    let top = lua_stack_top(lua);
    *(top as *mut f64) = value;
    *((top + 8) as *mut u32) = 3;
    *(lua as *mut usize).add(2) = top + 0x10;
}

pub(crate) unsafe fn lua_push_boolean(lua: usize, value: bool) {
    let top = lua_stack_top(lua);
    // Lua 5.3 stores a boolean as an int in the value union, tag 1.
    *(top as *mut u32) = value as u32;
    *((top + 8) as *mut u32) = 1;
    *(lua as *mut usize).add(2) = top + 0x10;
}

pub(crate) unsafe fn lua_push_table(lua: usize) -> usize {
    let create: unsafe extern "C" fn(usize) -> usize = game_fn(RVA_CREATE_TABLE);
    let table = create(lua);
    let top = lua_stack_top(lua);
    *(top as *mut usize) = table;
    *((top + 8) as *mut u32) = 0x45;
    *(lua as *mut usize).add(2) = top + 0x10;
    table
}

pub(crate) unsafe fn lua_rawseti(lua: usize, index: i64) {
    let f: unsafe extern "C" fn(usize, i32, i64) = game_fn(RVA_RAWSETI);
    f(lua, -2, index);
}

#[allow(dead_code)]
pub(crate) unsafe fn lua_push_string(lua: usize, value: &str) {
    let Ok(cstring) = CString::new(value) else {
        return;
    };
    let f: unsafe extern "C" fn(usize, *const i8) = game_fn(RVA_PUSH_STRING);
    f(lua, cstring.as_ptr());
}

/// Replicates `FUN_140332a50`'s component lookup: the component is carried as
/// an upvalue of every component-mod Lua function, reachable through the
/// pseudo-index `0xfff0b9d7`.
pub(crate) unsafe fn lua_component_handle(lua: usize) -> usize {
    let slot = lua_arg_slot(lua, FIRST_UPVALUE_INDEX);
    if slot.is_null() {
        return 0;
    }
    let tag = *(slot.add(8) as *const u32) & 0xf;
    match tag {
        2 => *(slot as *const usize),
        7 => *(slot as *const usize) + 0x28,
        _ => 0,
    }
}

pub(crate) unsafe fn lua_component(lua: usize) -> usize {
    let handle = lua_component_handle(lua);
    if handle == 0 {
        return 0;
    }
    read_usize_at(handle)
}

// ---------------------------------------------------------------------------
// temperature sampling
// ---------------------------------------------------------------------------
