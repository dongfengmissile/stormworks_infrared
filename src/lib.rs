//! Stormworks `infrared` plugin.
//!
//! Injects an `ir` global table into the component-mod Lua state and exposes
//! the game's own heat / temperature evaluator to component scripts:
//!
//! ```lua
//! ir.probe(x, y, z)                                  -> temperature (Celsius)
//! ir.scan(az, el, fov, range, rx, ry, vmin, vmax, axis)
//!     -> cells[1..rx*ry], az_err, el_err, strength
//! ```
//!
//! Two game functions are hooked:
//!   * `FUN_14032be50` (component-mod Lua API registrar) - so we can append
//!     the `ir` table using the game's own `register_table` helper.
//!   * `FUN_14030fd60` (temperature at a world point) - so we can capture the
//!     heat-container arguments the game passes in and then call the function
//!     ourselves for arbitrary sample points.
//!
//! Since `FUN_14030fd60` already applies the component heat sources, fires,
//! lava and base ambient temperature, the scan sees exactly the same heat the
//! stock Temperature Probe does.

#![cfg(windows)]
#![allow(non_snake_case, non_upper_case_globals, dead_code)]

use std::ffi::{c_void, CString};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use minhook::MinHook;
use windows_sys::Win32::Foundation::HINSTANCE;

use windows_sys::Win32::System::LibraryLoader::{
    DisableThreadLibraryCalls, GetModuleFileNameW, GetModuleHandleW,
};

/// Build this DLL was relocated against.
/// stormworks64.exe sha256:
/// f9206d85c82f4d02fd0ac391781d19c5c68394a9ffc48accca0e1f5966db8699
const GAME_BUILD_LABEL: &str = "f9206d85c82f";

// RVAs (image base 0x140000000).
const RVA_COMPONENT_LUA_REGISTRAR: usize = 0x0032_be50; // FUN_14032be50
const RVA_TEMPERATURE: usize = 0x0030_fd60; // FUN_14030fd60
const RVA_ARG_SLOT: usize = 0x0009_7920; // FUN_140097920
const RVA_PUSH_STRING: usize = 0x0009_7f60; // FUN_140097f60
const RVA_CREATE_TABLE: usize = 0x000b_a830; // FUN_1400ba830
const RVA_RAWSETI: usize = 0x0009_8570; // FUN_140098570
const RVA_REGISTER_TABLE: usize = 0x001e_22d0; // FUN_1401e22d0
const RVA_STRING_ALLOC: usize = 0x0017_cbb0; // FUN_14017cbb0
const RVA_STRING_FILL: usize = 0x0099_d3b0; // FUN_14099d3b0
const RVA_BASE_TEMP: usize = 0x0031_00a0; // FUN_1403100a0(point,..,daybyte)->ambient
const RVA_RAYCAST: usize = 0x001a_4160; // FUN_1401a4160(ctx,seg6d,out_hit,out_obj,grp,mask)->bool
const RVA_FIRES_BUILD: usize = 0x0030_a2f0; // FUN_14030a2f0 fire vertex builder (holder owns the fires ring)
const RVA_SCENE_TICK: usize = 0x005e_85e0; // FUN_1405e85e0 scene tick (arg1 = scene object; refreshes the day clock)
const RVA_SETTINGS: usize = 0x0029_4520; // FUN_140294520() -> settings singleton ([0] = day length, minutes)
const RVA_FRAME_UNIFORMS: usize = 0x006b_b400; // FUN_1406bb400 render-thread uniform fill; arg2 = the scene object
const FRAME_UNIFORMS_PROLOGUE: [u8; 32] = [
    0x40, 0x55, 0x53, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48, 0x8d, 0xac,
    0x24, 0xc8, 0x77, 0xff, 0xff, 0xb8, 0x58, 0x89, 0x00, 0x00, 0xe8, 0xf1, 0x77, 0x31, 0x00, 0x48,
];
const RVA_SUNROT: usize = 0x002bf_9e0; // FUN_1402bf9e0(day_percent_f32, dir3_out, mat_out) — the game's rotating sun
const RVA_SUN_UPDATE: usize = 0x005e_3270; // FUN_1405e3270(ctx, day_percent, flags, out) — per-tick sun update; out+0x198 = sun sprite world position
const SUN_UPDATE_PROLOGUE: [u8; 32] = [
    0x48, 0x8b, 0xc4, 0x55, 0x53, 0x56, 0x57, 0x41, 0x54, 0x41, 0x56, 0x41, 0x57, 0x48, 0x8d, 0x6c,
    0x24, 0xb0, 0x48, 0x81, 0xec, 0x50, 0x01, 0x00, 0x00, 0x0f, 0x29, 0x70, 0xb8, 0x0f, 0x29, 0x78,
];

// Occlusion tuning: ignore blockers within OCCLUDE_MIN_DIST of the sensor (its
// own airframe) and within OCCLUDE_END_SLACK of the emitter (its own hull edge).
const OCCLUDE_MIN_DIST: f64 = 1.0;
const OCCLUDE_END_SLACK: f64 = 2.0;

const FIRST_UPVALUE_INDEX: i32 = -1_001_001; // 0xfff0b9d7
const COMPONENT_MATRIX_OFFSET: usize = 0x1e8;

const REGISTRAR_PROLOGUE: [u8; 32] = [
    0x48, 0x89, 0x5c, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x55, 0x57, 0x41, 0x56, 0x48, 0x8d,
    0xac, 0x24, 0x50, 0xfc, 0xff, 0xff, 0x48, 0x81, 0xec, 0xb0, 0x04, 0x00, 0x00, 0x48, 0x8b, 0x05,
];
const TEMPERATURE_PROLOGUE: [u8; 32] = [
    0x48, 0x8b, 0xc4, 0x48, 0x89, 0x58, 0x10, 0x48, 0x89, 0x68, 0x18, 0x48, 0x89, 0x70, 0x20, 0x57,
    0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48, 0x81, 0xec, 0xb0, 0x00, 0x00, 0x00, 0x0f,
];
const FIRES_PROLOGUE: [u8; 32] = [
    0x48, 0x8b, 0xc4, 0x48, 0x89, 0x58, 0x08, 0x48, 0x89, 0x70, 0x18, 0x48, 0x89, 0x78, 0x20, 0x55,
    0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48, 0x8d, 0x6c, 0x24, 0xa0, 0x48, 0x81, 0xec,
];
const SETTINGS_PROLOGUE: [u8; 32] = [
    0x48, 0x83, 0xec, 0x28, 0xba, 0x04, 0x00, 0x00, 0x00, 0x65, 0x48, 0x8b, 0x04, 0x25, 0x58, 0x00,
    0x00, 0x00, 0x48, 0x8b, 0x08, 0x8b, 0x04, 0x0a, 0x39, 0x05, 0xa6, 0x8c, 0xa6, 0x00, 0x7e, 0x33,
];
const SCENE_TICK_PROLOGUE: [u8; 32] = [
    0x48, 0x89, 0x5c, 0x24, 0x08, 0x48, 0x89, 0x6c, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x57,
    0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48, 0x83, 0xec, 0x20, 0x80, 0x3d, 0x51, 0x1e,
];

const TAU: f64 = std::f64::consts::TAU;

static SELF_MODULE: AtomicUsize = AtomicUsize::new(0);
static MODULE_BASE: AtomicUsize = AtomicUsize::new(0);
static ORIG_REGISTRAR: AtomicUsize = AtomicUsize::new(0);
static ORIG_TEMPERATURE: AtomicUsize = AtomicUsize::new(0);
static ORIG_FIRES_BUILD: AtomicUsize = AtomicUsize::new(0);
static FIRES_HOLDER: AtomicUsize = AtomicUsize::new(0);
static ORIG_SCENE_TICK: AtomicUsize = AtomicUsize::new(0);
static SCENE: AtomicUsize = AtomicUsize::new(0);
static HOOKS_INSTALLED: AtomicBool = AtomicBool::new(false);
static REGISTRAR_HITS: AtomicUsize = AtomicUsize::new(0);
static LOG_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

// Arguments seen by FUN_14030fd60. p2 is the (global) heat-source container and
// p9 is the owner filter the game compares under equality to record+0x120, so
// replaying them only ever exposes one owner's heat sources. We therefore cache
// the container/args per distinct container and evaluate every record directly.
// Stored lock-free because the scan samples in a tight loop.
const MAX_VARIANTS: usize = 4;
static TEMP_READY: AtomicBool = AtomicBool::new(false);
static VARIANT_ARGS: [AtomicUsize; 8 * MAX_VARIANTS] =
    [const { AtomicUsize::new(0) }; 8 * MAX_VARIANTS];
static VARIANT_COUNT: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
struct FnPair {
    name: *const u8,
    func: usize,
}

// Diagnostics: log the first temperature capture, and a throttled summary of
// the raw temperatures a scan actually sees (for calibrating the vis window).
static TEMP_FIRST: AtomicBool = AtomicBool::new(false);
static SCAN_COUNT: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// logging
// ---------------------------------------------------------------------------

fn set_log_path(path: PathBuf) {
    // Re-initialise the log every launch so old sessions do not mix in.
    let _ = fs::write(&path, b"");
    if let Ok(mut slot) = LOG_PATH.lock() {
        *slot = Some(path);
    }
}

fn log_line(message: &str) {
    if let Ok(slot) = LOG_PATH.lock() {
        if let Some(path) = slot.as_ref() {
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(file, "{message}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// module + game function helpers
// ---------------------------------------------------------------------------

unsafe fn game_fn<T: Copy>(rva: usize) -> T {
    let address = MODULE_BASE.load(Ordering::SeqCst).wrapping_add(rva);
    std::mem::transmute_copy(&address)
}

fn bytes_match(address: usize, expected: &[u8]) -> bool {
    unsafe {
        let actual = std::slice::from_raw_parts(address as *const u8, expected.len());
        actual == expected
    }
}

fn self_module_path() -> Option<PathBuf> {
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

unsafe fn lua_arg_slot(lua: usize, index: i32) -> *mut u8 {
    let f: unsafe extern "C" fn(usize, i32) -> *mut u8 = game_fn(RVA_ARG_SLOT);
    f(lua, index)
}

unsafe fn lua_arg_number(lua: usize, index: i32) -> Option<f64> {
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

unsafe fn lua_stack_top(lua: usize) -> usize {
    *(lua as *const usize).add(2)
}

unsafe fn lua_push_number(lua: usize, value: f64) {
    let top = lua_stack_top(lua);
    *(top as *mut f64) = value;
    *((top + 8) as *mut u32) = 3;
    *(lua as *mut usize).add(2) = top + 0x10;
}

unsafe fn lua_push_table(lua: usize) -> usize {
    let create: unsafe extern "C" fn(usize) -> usize = game_fn(RVA_CREATE_TABLE);
    let table = create(lua);
    let top = lua_stack_top(lua);
    *(top as *mut usize) = table;
    *((top + 8) as *mut u32) = 0x45;
    *(lua as *mut usize).add(2) = top + 0x10;
    table
}

unsafe fn lua_rawseti(lua: usize, index: i64) {
    let f: unsafe extern "C" fn(usize, i32, i64) = game_fn(RVA_RAWSETI);
    f(lua, -2, index);
}

#[allow(dead_code)]
unsafe fn lua_push_string(lua: usize, value: &str) {
    let Ok(cstring) = CString::new(value) else {
        return;
    };
    let f: unsafe extern "C" fn(usize, *const i8) = game_fn(RVA_PUSH_STRING);
    f(lua, cstring.as_ptr());
}

/// Replicates `FUN_140332a50`'s component lookup: the component is carried as
/// an upvalue of every component-mod Lua function, reachable through the
/// pseudo-index `0xfff0b9d7`.
unsafe fn lua_component_handle(lua: usize) -> usize {
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

unsafe fn lua_component(lua: usize) -> usize {
    let handle = lua_component_handle(lua);
    if handle == 0 {
        return 0;
    }
    read_usize_at(handle)
}

// ---------------------------------------------------------------------------
// temperature sampling
// ---------------------------------------------------------------------------

unsafe fn read_usize_at(addr: usize) -> usize {
    *(addr as *const usize)
}
unsafe fn read_u32_at(addr: usize) -> u32 {
    *(addr as *const u32)
}
unsafe fn read_f32_at(addr: usize) -> f32 {
    *(addr as *const f32)
}
unsafe fn read_f64_at(addr: usize) -> f64 {
    *(addr as *const f64)
}

/// Diagnostic: describe the cached heat-source container (ring list) and a few
/// records, so we can tell whether a nearby engine is even a heat-field source.
/// Diagnostic dump of one heat container variant (logging builds only).
#[cfg(feature = "logging")]
unsafe fn log_heat_container(c: usize) {
    if c == 0 {
        log_line("heat container: NULL");
        return;
    }
    let slots = read_usize_at(c + 0x20);
    let cap = read_u32_at(c + 0x28);
    let start = read_u32_at(c + 0x2c);
    let count = read_u32_at(c + 0x30);
    log_line(&format!(
        "heat container {:#x}: slots={:#x} cap={} start={} count={}",
        c, slots, cap, start, count
    ));
    if slots < 0x10000 || cap == 0 || count == 0 {
        return;
    }
    let dump = count.min(32);
    for i in 0..dump {
        let idx = ((start as usize + i as usize) % cap as usize) as usize;
        let holder = read_usize_at(slots + idx * 8);
        if !(0x10000..0x7fff_ffff_ffff).contains(&holder) {
            continue;
        }
        let rec = read_usize_at(holder);
        if !(0x10000..0x7fff_ffff_ffff).contains(&rec) {
            continue;
        }
        let mode = *(rec as *const u8);
        let cx = read_f64_at(rec + 0x68);
        let cy = read_f64_at(rec + 0x70);
        let cz = read_f64_at(rec + 0x78);
        let radius = read_f32_at(rec + 0x108);
        let factor = read_f32_at(rec + 0x114);
        let temp = read_f32_at(rec + 0x118);
        let owner = read_usize_at(rec + 0x120);
        log_line(&format!(
            "  heat[{idx}] mode={mode} c=({cx:.1},{cy:.1},{cz:.1}) r/ext={radius:.2} f={factor:.3} T={temp:.1} owner={owner:#x}"
        ));
    }
}

fn cache_variant(args: &[usize; 8]) {
    if args[0] == 0 {
        return;
    }
    let count = VARIANT_COUNT.load(Ordering::Relaxed).min(MAX_VARIANTS);
    for i in 0..count {
        let base = i * 8;
        if VARIANT_ARGS[base].load(Ordering::Relaxed) == args[0] {
            for j in 0..8 {
                VARIANT_ARGS[base + j].store(args[j], Ordering::Relaxed);
            }
            return;
        }
    }
    let slot = count % MAX_VARIANTS;
    let base = slot * 8;
    for j in 0..8 {
        VARIANT_ARGS[base + j].store(args[j], Ordering::Relaxed);
    }
    if count < MAX_VARIANTS {
        VARIANT_COUNT.store(count + 1, Ordering::Relaxed);
    }
}

fn variant_args(i: usize) -> [usize; 8] {
    let base = i * 8;
    let mut a = [0usize; 8];
    for j in 0..8 {
        a[j] = VARIANT_ARGS[base + j].load(Ordering::Relaxed);
    }
    a
}

/// Heat-source influence at a point, replicated from `FUN_14030bd10` so the
/// scan does not depend on calling that function directly:
///   sphere: t = (dist(center,+0x68..) - radius(+0x108)) / (T(+0x118) * 0.125)
///   box:    t = max(0, axis dist - extent/2) / (T * 0.125)
///   return: (1 - clamp(t)) * factor(+0x114)
#[inline]
unsafe fn influence_at(rec: usize, px: f64, py: f64, pz: f64) -> f32 {
    let factor = read_f32_at(rec + 0x114);
    let temp = read_f32_at(rec + 0x118);
    if factor <= 0.0 || temp <= 0.0 {
        return 0.0;
    }
    let denom = temp * 0.125;
    let mode = *(rec as *const u8);
    let t = if mode != 0 {
        let a = ((px * read_f64_at(rec + 0x88)
            + py * read_f64_at(rec + 0xa8)
            + pz * read_f64_at(rec + 0xc8)
            + read_f64_at(rec + 0xe8))
            .abs() as f32
            - read_f32_at(rec + 0x108) * 0.5)
            .max(0.0);
        let b = ((px * read_f64_at(rec + 0x90)
            + py * read_f64_at(rec + 0xb0)
            + pz * read_f64_at(rec + 0xd0)
            + read_f64_at(rec + 0xf0))
            .abs() as f32
            - read_f32_at(rec + 0x10c) * 0.5)
            .max(0.0);
        let c = ((px * read_f64_at(rec + 0x98)
            + py * read_f64_at(rec + 0xb8)
            + pz * read_f64_at(rec + 0xd8)
            + read_f64_at(rec + 0xf8))
            .abs() as f32
            - read_f32_at(rec + 0x110) * 0.5)
            .max(0.0);
        a.max(b).max(c) / denom
    } else {
        let dx = px - read_f64_at(rec + 0x68);
        let dy = py - read_f64_at(rec + 0x70);
        let dz = pz - read_f64_at(rec + 0x78);
        let dist = ((dx * dx + dy * dy + dz * dz).sqrt()) as f32;
        (dist - read_f32_at(rec + 0x108)) / denom
    };
    (1.0 - t.clamp(0.0, 1.0)) * factor
}

/// Max heat contribution over every record in one container, ignoring the
/// per-owner filter: `influence(record, point) * record.temperature`.
unsafe fn max_heat_in_container(c: usize, px: f64, py: f64, pz: f64) -> f32 {
    if c < 0x10000 {
        return 0.0;
    }
    let slots = read_usize_at(c + 0x20);
    let cap = read_u32_at(c + 0x28) as usize;
    let start = read_u32_at(c + 0x2c) as usize;
    let count = read_u32_at(c + 0x30) as usize;
    if slots < 0x10000 || cap == 0 || count == 0 {
        return 0.0;
    }
    let mut best = 0.0f32;
    for i in 0..count {
        let idx = (start + i) % cap;
        let holder = read_usize_at(slots + idx * 8);
        if !(0x10000..0x7fff_ffff_ffff).contains(&holder) {
            continue;
        }
        let rec = read_usize_at(holder);
        if !(0x10000..0x7fff_ffff_ffff).contains(&rec) {
            continue;
        }
        let influence = influence_at(rec, px, py, pz);
        if influence > 0.0 {
            let contrib = influence * read_f32_at(rec + 0x118);
            if contrib > best {
                best = contrib;
            }
        }
    }
    best
}

unsafe fn temperature_at(x: f64, y: f64, z: f64) -> f32 {
    if !TEMP_READY.load(Ordering::SeqCst) {
        return 20.0;
    }
    let point = [x, y, z];
    let pptr = point.as_ptr() as usize;

    // The game's own function, for fires / lava / its exact blend.
    let mut best = 20.0f32;
    let original = ORIG_TEMPERATURE.load(Ordering::SeqCst);
    let count = VARIANT_COUNT.load(Ordering::Relaxed).min(MAX_VARIANTS);
    if original != 0 && count > 0 {
        let f: unsafe extern "C" fn(
            usize,
            usize,
            usize,
            usize,
            usize,
            usize,
            usize,
            usize,
            usize,
        ) -> f32 = std::mem::transmute(original);
        let a = variant_args(0);
        best = f(pptr, a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7]);
    }

    if count > 0 {
        let base_fn: unsafe extern "C" fn(usize, usize, usize, usize, u8) -> f32 =
            game_fn(RVA_BASE_TEMP);
        let a0 = variant_args(0);
        let base = base_fn(pptr, a0[1], a0[2], a0[2], a0[6] as u8);
        let mut heat = 0.0f32;
        for i in 0..count {
            let a = variant_args(i);
            let h = max_heat_in_container(a[0], x, y, z);
            if h > heat {
                heat = h;
            }
        }
        let combined = base + heat;
        if combined > best {
            best = combined;
        }
    }
    best
}

// ---------------------------------------------------------------------------
// hooks
// ---------------------------------------------------------------------------

extern "C" fn ir_registrar_hook(context: usize) {
    let original = ORIG_REGISTRAR.load(Ordering::SeqCst);
    if original != 0 {
        let f: unsafe extern "C" fn(usize) = unsafe { std::mem::transmute(original) };
        unsafe { f(context) };
    }
    unsafe { register_ir_table(context) };
    REGISTRAR_HITS.fetch_add(1, Ordering::Relaxed);
}

extern "C" fn ir_temperature_hook(
    p1: usize,
    p2: usize,
    p3: usize,
    p4: usize,
    p5: usize,
    p6: usize,
    p7: usize,
    p8: usize,
    p9: usize,
) -> f32 {
    cache_variant(&[p2, p3, p4, p5, p6, p7, p8, p9]);
    TEMP_READY.store(true, Ordering::SeqCst);
    if !TEMP_FIRST.swap(true, Ordering::SeqCst) {
        log_line("temperature hook first call: heat containers cached");
    }

    let original = ORIG_TEMPERATURE.load(Ordering::SeqCst);
    if original == 0 {
        return 0.0;
    }
    let f: unsafe extern "C" fn(
        usize,
        usize,
        usize,
        usize,
        usize,
        usize,
        usize,
        usize,
        usize,
    ) -> f32 = unsafe { std::mem::transmute(original) };
    unsafe { f(p1, p2, p3, p4, p5, p6, p7, p8, p9) }
}

/// Cache the fires-system holder from the fire vertex builder (its arg 1 owns
/// the fires ring at +0x11d8..+0x11e8, the same one the temperature function
/// reads as its fires argument). This makes flares and burning fires visible
/// even before any temperature evaluation (engine/probe) has run this session.
extern "C" fn ir_fires_hook(
    p1: usize,
    p2: usize,
    p3: usize,
    p4: usize,
    p5: usize,
    p6: usize,
) {
    if p1 >= 0x10000 {
        FIRES_HOLDER.store(p1, Ordering::Relaxed);
        static FIRST: AtomicBool = AtomicBool::new(false);
        if !FIRST.swap(true, Ordering::Relaxed) {
            log_line("fires builder hook first call: fires holder cached");
        }
    }
    let original = ORIG_FIRES_BUILD.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    let f: unsafe extern "C" fn(usize, usize, usize, usize, usize, usize) =
        unsafe { std::mem::transmute(original) };
    unsafe { f(p1, p2, p3, p4, p5, p6) }
}

/// Register the `ir` global into the component-mod Lua state, exactly the way
/// the game registers its own `matrix` / `parser` / `component` tables.
unsafe fn register_ir_table(context: usize) {
    if context == 0 {
        return;
    }
    let alloc: unsafe extern "C" fn(*mut usize, u32) = game_fn(RVA_STRING_ALLOC);
    let fill: unsafe extern "C" fn(usize, *const u8, u32) = game_fn(RVA_STRING_FILL);
    let register: unsafe extern "C" fn(usize, *const usize, *const FnPair, usize) =
        game_fn(RVA_REGISTER_TABLE);

    let mut name_object: usize = 0;
    alloc(&mut name_object, 3);
    if name_object == 0 {
        log_line("ir registration: name allocation failed");
        return;
    }
    fill(name_object, b"ir\0".as_ptr(), 3);

    let pairs = [
        FnPair {
            name: b"scan\0".as_ptr(),
            func: ir_scan as *const () as usize,
        },
        FnPair {
            name: b"probe\0".as_ptr(),
            func: ir_probe as *const () as usize,
        },
        FnPair {
            name: std::ptr::null(),
            func: 0,
        },
    ];

    register(context, &name_object as *const usize, pairs.as_ptr(), context + 0x28);
}

// ---------------------------------------------------------------------------
// injected Lua functions
// ---------------------------------------------------------------------------

extern "C" fn ir_probe(lua: *mut c_void) -> i32 {
    let lua = lua as usize;
    unsafe {
        let x = lua_arg_number(lua, 1).unwrap_or(0.0);
        let y = lua_arg_number(lua, 2).unwrap_or(0.0);
        let z = lua_arg_number(lua, 3).unwrap_or(0.0);
        let temperature = temperature_at(x, y, z) as f64;
        lua_push_number(lua, temperature);
    }
    1
}

// One heat source as seen by the IR sensor.
struct IrSource {
    x: f64,
    y: f64,
    z: f64,
    r_eff: f64,  // effective emitting radius (m)
    flux: f64,   // irradiance at the sensor, 1.0 == 127 C / 1 m source at 10 m
    az: f64,     // direction in the sensor frame (rad)
    el: f64,
    theta: f64,  // angular radius (rad)
    sky: bool,   // sky source (the sun): not range-culled
}

/// Emission model: MWIR (3-5 um) in-band blackbody radiance, the band real IR
/// missile seekers use. A 400 C furnace peaks at 4.3 um (Wien) while a 40 C
/// engine radiates almost nothing in-band, so hot sources get their realistic
/// ~356x brightness over a warm engine instead of a tame (T/400 K)^4 scaling.
/// flux = factor * L_band(T_K) * r^2 / d^2 / MWIR_REF, with MWIR_REF calibrated
/// so a 40 C / r=0.7 record at 10 m reads ~0.29 (the old model's anchor).
const MWIR_ANCHOR_FLUX: f64 = 0.29;
const FIRE_T_K: f64 = 1273.15; // ~1000 C flame, scaled by the fire's intensity

/// Planck spectral radiance integrated over 3..5 um (W/m^2/sr), 24-point
/// Simpson. Cheap: evaluated once per source per scan, not per cell.
fn band_radiance(t_k: f64) -> f64 {
    const C1: f64 = 1.19104e-16; // 2 h c^2
    const C2: f64 = 1.4388e-2; // h c / k
    if !(100.0..=4000.0).contains(&t_k) {
        return 0.0;
    }
    let (a, b) = (3e-6f64, 5e-6f64);
    let n = 24usize; // even
    let h = (b - a) / n as f64;
    let mut s = 0.0f64;
    for i in 0..=n {
        let lam = a + i as f64 * h;
        let x = C2 / (lam * t_k);
        let f = C1 / lam.powi(5) / x.exp_m1();
        let w = if i == 0 || i == n {
            1.0
        } else if i % 2 == 1 {
            4.0
        } else {
            2.0
        };
        s += f * w;
    }
    s * h / 3.0
}

/// Local weather. The game keeps its weather object on the scene at
/// scene+0xbade8 (the day clock sits just after it at +0xbaec0): the live,
/// smoothed fog factor is at +0x90 and rain at +0x98, while the cheat menu /
/// server.setWeather write target values at +0x94 / +0x9c which the live
/// values chase by 1% per tick (see FUN_1402c0810 / FUN_1402c1400). Reading
/// the max of live and target covers the transition too. Wind is at
/// +0xa0 (live) / +0xa4 (target), direction at +0xa8.
unsafe fn weather_fog_rain() -> (f32, f32) {
    let scene = SCENE.load(Ordering::Relaxed);
    if scene < 0x10000 {
        return (0.0, 0.0);
    }
    let w = scene + 0xbade8;
    let fog = read_f32_at(w + 0x90).max(read_f32_at(w + 0x94));
    let rain = read_f32_at(w + 0x98).max(read_f32_at(w + 0x9c));
    let fog = if fog.is_finite() { fog.clamp(0.0, 1.0) } else { 0.0 };
    let rain = if rain.is_finite() { rain.clamp(0.0, 1.0) } else { 0.0 };
    (fog, rain)
}

/// MWIR (3-5 um) extinction by weather. Fog dominates: its droplets are close
/// to the band's wavelength, so they scatter and absorb strongly, while rain
/// drops are far larger and attenuate much less per unit of water. A fully
/// fogged world blinds a long-range seeker (1 km -> ~0.06% transmittance)
/// while barely affecting a few hundred metres.
const FOG_SIGMA_PER_M: f64 = 0.0075;
const RAIN_SIGMA_PER_M: f64 = 0.0020;
/// Fog along a sky path (the sun) is deliberately gentler than fog along a
/// line of sight to a surface source: a real seeker keeps seeing the sun as a
/// diffuse glow through fairly thick fog (forward scattering dominates), while
/// a point target washes out. Rain is left at the surface coefficient.
const FOG_SIGMA_SKY_PER_M: f64 = 0.0070;
/// The sun emitter is modelled 10 km away but the path that actually fogs it
/// is the weather layer, so sky paths are clamped before applying extinction.
const SKY_PATH_CLAMP_M: f64 = 1500.0;

fn weather_transmittance(fog: f32, rain: f32, path_m: f64, sky: bool) -> f64 {
    let fog_sigma = if sky { FOG_SIGMA_SKY_PER_M } else { FOG_SIGMA_PER_M };
    let sigma = fog as f64 * fog_sigma + rain as f64 * RAIN_SIGMA_PER_M;
    if sigma <= 0.0 {
        return 1.0;
    }
    let path = if sky { path_m.min(SKY_PATH_CLAMP_M) } else { path_m };
    (-sigma * path).exp()
}

/// Normalisation constant derived once from the 40 C anchor (see above).
fn mwir_ref() -> f64 {
    static REF: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *REF.get_or_init(|| {
        band_radiance(313.15) * 0.7 * 0.7 / (10.0 * 10.0) / MWIR_ANCHOR_FLUX
    })
}

/// Walk a heat container and emit every record as an IrSource, applying the
/// radiometric falloff. Records are evaluated regardless of their owner (the
/// game's own evaluation filters by owner, which would hide other vehicles).
unsafe fn collect_ir_sources(
    c: usize,
    origin: &Vec3,
    range: f64,
    out: &mut Vec<IrSource>,
) {
    if c < 0x10000 {
        return;
    }
    let slots = read_usize_at(c + 0x20);
    let cap = read_u32_at(c + 0x28) as usize;
    let start = read_u32_at(c + 0x2c) as usize;
    let count = read_u32_at(c + 0x30) as usize;
    if slots < 0x10000 || cap == 0 || count == 0 {
        return;
    }
    let (wx_fog, wx_rain) = weather_fog_rain();
    for i in 0..count {
        let idx = (start + i) % cap;
        let holder = read_usize_at(slots + idx * 8);
        if !(0x10000..0x7fff_ffff_ffff).contains(&holder) {
            continue;
        }
        let rec = read_usize_at(holder);
        if !(0x10000..0x7fff_ffff_ffff).contains(&rec) {
            continue;
        }
        let mode = *(rec as *const u8);
        let factor = read_f32_at(rec + 0x114) as f64;
        let temp = read_f32_at(rec + 0x118) as f64;
        if factor <= 0.0 || temp <= 0.0 {
            continue;
        }
        let (cx, cy, cz, r_eff) = if mode != 0 {
            // Oriented box: matrix at +0x88 (translation column +0xe8/+0xf0/+0xf8),
            // full extents at +0x108/+0x10c/+0x110.
            let e0 = read_f32_at(rec + 0x108) as f64;
            let e1 = read_f32_at(rec + 0x10c) as f64;
            let e2 = read_f32_at(rec + 0x110) as f64;
            (
                read_f64_at(rec + 0xe8),
                read_f64_at(rec + 0xf0),
                read_f64_at(rec + 0xf8),
                0.5 * e0.max(e1).max(e2),
            )
        } else {
            (
                read_f64_at(rec + 0x68),
                read_f64_at(rec + 0x70),
                read_f64_at(rec + 0x78),
                read_f32_at(rec + 0x108) as f64,
            )
        };
        if r_eff <= 0.001 {
            continue;
        }
        let dx = cx - origin.0;
        let dy = cy - origin.1;
        let dz = cz - origin.2;
        let dist = (dx * dx + dy * dy + dz * dz).sqrt();
        let surface = (dist - r_eff).max(1.0);
        if surface > range {
            continue;
        }
        let t_k = temp + 273.15;
        let ref_const = mwir_ref();
        let flux = factor * band_radiance(t_k) * (r_eff * r_eff)
            / (surface * surface)
            / ref_const
            * weather_transmittance(wx_fog, wx_rain, surface, false);
        out.push(IrSource {
            x: cx,
            y: cy,
            z: cz,
            r_eff,
            flux,
            az: 0.0,
            el: 0.0,
            theta: (r_eff / dist.max(0.5)).atan(),
            sky: false,
        });
    }
}

/// Express a source's direction in the sensor frame (azimuth/elevation).
fn project_source(
    s: &mut IrSource,
    origin: Vec3,
    forward: Vec3,
    right: Vec3,
    up: Vec3,
) {
    let dx = s.x - origin.0;
    let dy = s.y - origin.1;
    let dz = s.z - origin.2;
    let len = (dx * dx + dy * dy + dz * dz).sqrt().max(1e-6);
    let (dx, dy, dz) = (dx / len, dy / len, dz / len);
    let fx = dx * forward.0 + dy * forward.1 + dz * forward.2;
    let rx = dx * right.0 + dy * right.1 + dz * right.2;
    let ux = dx * up.0 + dy * up.1 + dz * up.2;
    s.az = rx.atan2(fx);
    s.el = ux.clamp(-1.0, 1.0).asin();
}

/// Base ambient temperature at a world point (no heat sources added).
unsafe fn base_ambient_at(origin: Vec3) -> f32 {
    if !TEMP_READY.load(Ordering::SeqCst) {
        return 20.0;
    }
    if VARIANT_COUNT.load(Ordering::Relaxed) == 0 {
        return 20.0;
    }
    let a0 = variant_args(0);
    let base_fn: unsafe extern "C" fn(usize, usize, usize, usize, u8) -> f32 =
        game_fn(RVA_BASE_TEMP);
    let point = [origin.0, origin.1, origin.2];
    base_fn(
        point.as_ptr() as usize,
        a0[1],
        a0[2],
        a0[2],
        a0[6] as u8,
    )
}

/// World-space raycast through the game's physics world (Bullet rayTest).
/// Returns the distance to the first hit in metres, or None.
unsafe fn raycast_hit(ctx: usize, origin: Vec3, target: Vec3) -> Option<f64> {
    if ctx == 0 {
        return None;
    }
    let ray_fn: unsafe extern "C" fn(usize, *const f64, *mut f64, *mut usize, u32, u32) -> u8 =
        game_fn(RVA_RAYCAST);
    let seg = [
        origin.0, origin.1, origin.2, target.0, target.1, target.2,
    ];
    if (seg[3] - seg[0]) * (seg[3] - seg[0])
        + (seg[4] - seg[1]) * (seg[4] - seg[1])
        + (seg[5] - seg[2]) * (seg[5] - seg[2])
        < 1e-6
    {
        return None;
    }
    let mut hit_point = [0.0f64; 3];
    let mut hit_object: usize = 0;
    let hit = ray_fn(
        ctx,
        seg.as_ptr(),
        hit_point.as_mut_ptr(),
        &mut hit_object,
        0x7f,
        0xff,
    );
    if hit == 0 {
        return None;
    }
    let dx = hit_point[0] - seg[0];
    let dy = hit_point[1] - seg[1];
    let dz = hit_point[2] - seg[2];
    Some((dx * dx + dy * dy + dz * dz).sqrt())
}

/// The sun as a directional MWIR emitter using the game's own rotating sun
/// light (FUN_1402bf9e0, consumed by FUN_1405e3270 as the sun matrix and the
/// 18 km sun sprite position): the direction rotates around the fixed axis
/// normalize((0, 1, -0.5)) by theta = (day_percent - 0.5) * 2*pi, composed
/// with Rx(-2). The game's sun_light_direction uniform is the THIRD ROW of
/// that product (pointing from the sun toward the scene, the shader uses
/// -dot(dir, normal)); the direction TO the sun is its negation. The arc is
/// exactly at the horizon at 06:00, ~65 deg up (tilted toward +z) at noon,
/// horizon at 18:00 — the emitter exists only while the sun is up.
/// In-band solar irradiance is ~150x the engine anchor (SUN_FLUX).
const SUN_FLUX: f64 = 45.0;
const SUN_DIST: f64 = 10000.0;
const SUN_R_EFF: f64 = 44.0; // ~0.25 deg angular size at 10 km

unsafe fn scene_env() -> usize {
    let s = SCENE.load(Ordering::Relaxed);
    if s >= 0x10000 {
        return s;
    }
    if VARIANT_COUNT.load(Ordering::Relaxed) > 0 {
        let v = variant_args(0)[1];
        if v >= 0x10000 {
            return v;
        }
    }
    FIRES_HOLDER.load(Ordering::Relaxed) // last resort (has fires, but no scene clock)
}

unsafe fn add_sun_source(origin: Vec3, sources: &mut Vec<IrSource>) {
    SUN_LOG.store(0, Ordering::Relaxed);
    let env = scene_env();
    if env < 0x10000 {
        return; // no scene yet (dump prints "waiting")
    }
    let settings_fn: unsafe extern "C" fn() -> usize = game_fn(RVA_SETTINGS);
    let settings = settings_fn();
    if settings < 0x10000 {
        return;
    }
    let day_min = read_u32_at(settings) as u64;
    if day_min == 0 || day_min > 24 * 60 {
        return;
    }
    let day_ticks = day_min * 3600;
    let clock = read_u32_at(env + 0xbaec0) as u64;
    let percent = (clock % day_ticks) as f64 / day_ticks as f64;

    // Camera-independent sun direction straight from the game: FUN_1402bf9e0
    // is the exact function that builds the rotating sun (it feeds the sun
    // light matrix, the sky sprite at 18 km and the 37449100 m sun ray used
    // by the sky/atmosphere code). Pure math, no game state — safe to call.
    let sunrot: unsafe extern "C" fn(f32, *mut f32, *mut f32) = game_fn(RVA_SUNROT);
    let mut gdir = [0f32; 4];
    let mut gmat = [0f32; 16];
    sunrot(percent as f32, gdir.as_mut_ptr(), gmat.as_mut_ptr());
    let vx = gdir[0] as f64;
    let vy = gdir[1] as f64;
    let vz = gdir[2] as f64;
    let vn = (vx * vx + vy * vy + vz * vz).sqrt();
    if vn < 1.0e-6 {
        SUN_LOG.store(0, Ordering::Relaxed);
        return;
    }
    // The game's vector points FROM the sun (the light travels along it, and
    // the sun ray origin is camera - dir * 37449100); the seeker needs the
    // direction TO the sun.
    let (dx, dy, dz) = (-vx / vn, -vy / vn, -vz / vn);
    let el = dy.asin();
    if el < 0.0 {
        SUN_LOG.store(1, Ordering::Relaxed); // 1 = sun below the horizon
        return;
    }
    let az_deg = dz.atan2(dx).to_degrees();
    SUN_EL.store((el.to_degrees() * 10.0) as isize, Ordering::Relaxed);
    SUN_AZ.store((az_deg * 10.0) as isize, Ordering::Relaxed);
    SUN_PCT.store(
        (percent * 1000.0).clamp(0.0, 1_000_000.0) as usize,
        Ordering::Relaxed,
    );
    SUN_SCENE_LOG.store(env, Ordering::Relaxed);
    SUN_CLOCK_LOG.store(clock as usize, Ordering::Relaxed);
    let (wx_fog, wx_rain) = weather_fog_rain();
    sources.push(IrSource {
        x: origin.0 + dx * SUN_DIST,
        y: origin.1 + dy * SUN_DIST,
        z: origin.2 + dz * SUN_DIST,
        r_eff: SUN_R_EFF,
        flux: SUN_FLUX * weather_transmittance(wx_fog, wx_rain, SUN_DIST, true),
        az: 0.0,
        el: 0.0,
        theta: (SUN_R_EFF / SUN_DIST).atan(),
        sky: true,
    });
    SUN_LOG.store((percent * 1000.0).clamp(0.0, 1_000_000.0) as usize + 2, Ordering::Relaxed);
}

/// Diagnostics captured for the throttled sun dump.
static SUN_SCENE_LOG: AtomicUsize = AtomicUsize::new(0);
static SUN_CLOCK_LOG: AtomicUsize = AtomicUsize::new(0);

/// Hook on the game's per-tick sun update FUN_1405e3270: run the original
/// first (it writes the sun sprite position), then capture that position.
///
/// ABI note: the day percent arrives in XMM1 (the callee saves XMM1 and
/// feeds it to FUN_1402bf9e0), so the real signature carries another
/// float-class parameter ahead of it that the decompiler lost. Declaring
/// TWO float parameters keeps both XMM0 and XMM1 round-tripped unchanged
/// under either reading, so the trampoline always sees the exact incoming
/// register state; whichever float is in 0..1 is the day percent.
extern "C" fn ir_sun_update_hook(p1: usize, f0: f32, f1: f32, p3: u32, p4: usize) {
    let original = ORIG_SUN_UPDATE.load(Ordering::SeqCst);
    if original != 0 {
        let f: unsafe extern "C" fn(usize, f32, f32, u32, usize) =
            unsafe { std::mem::transmute(original) };
        unsafe { f(p1, f0, f1, p3, p4) };
    }
    if p4 >= 0x10000 {
        let x = unsafe { read_f64_at(p4 + 0x198) };
        let y = unsafe { read_f64_at(p4 + 0x1a0) };
        let z = unsafe { read_f64_at(p4 + 0x1a8) };
        let n = (x * x + y * y + z * z).sqrt();
        if n > 100.0 && n < 1.0e9 {
            if let Ok(mut g) = SUN_DIR.lock() {
                g[0] = x;
                g[1] = y;
                g[2] = z;
            }
            let pct_f = if (0.0..=1.0).contains(&f1) {
                f1
            } else if (0.0..=1.0).contains(&f0) {
                f0
            } else {
                0.0
            };
            SUN_PCT.store(
                (pct_f as f64 * 1000.0).clamp(0.0, 1_000_000.0) as usize,
                Ordering::Relaxed,
            );
            if !SUN_DIR_VALID.swap(true, Ordering::Relaxed) {
                log_line(&format!(
                    "sun update hook first call: sprite=({:.0},{:.0},{:.0}) f0={:.4} f1={:.4}",
                    x, y, z, f0, f1
                ));
            }
        }
    }
}

static ORIG_SUN_UPDATE: AtomicUsize = AtomicUsize::new(0);
/// Hook on the per-frame render uniform fill FUN_1406bb400.  It runs on the
/// render thread every frame (unlike the sim-side scene updates, which may
/// only run at world load), and its arg 2 IS the scene object — the very
/// same object whose clock subsystem it touches at +0xbaeb8.  Capturing the
/// scene here makes the day clock and sky state available no matter
/// when the plugin was injected.
///
/// ABI: 15 integer-class arguments.  All are round-tripped unchanged.
#[allow(clippy::too_many_arguments)]
extern "C" fn ir_frame_uniforms_hook(
    p1: usize,
    p2: usize,
    p3: *mut i32,
    p4: *mut f64,
    p5: usize,
    p6: u32,
    p7: i8,
    p8: u8,
    p9: u8,
    p10: usize,
    p11: *mut f64,
    p12: u8,
    p13: u8,
    p14: u8,
    p15: usize,
) {
    let original = ORIG_FRAME_UNIFORMS.load(Ordering::SeqCst);
    if original != 0 {
        let f: unsafe extern "C" fn(
            usize,
            usize,
            *mut i32,
            *mut f64,
            usize,
            u32,
            i8,
            u8,
            u8,
            usize,
            *mut f64,
            u8,
            u8,
            u8,
            usize,
        ) = unsafe { std::mem::transmute(original) };
        unsafe { f(p1, p2, p3, p4, p5, p6, p7, p8, p9, p10, p11, p12, p13, p14, p15) };
    }
    if p2 >= 0x10000 {
        let prev = SCENE.swap(p2, Ordering::Relaxed);
        if prev != p2 {
            static ONCE: AtomicBool = AtomicBool::new(false);
            if !ONCE.swap(true, Ordering::Relaxed) {
                let clock = unsafe { read_u32_at(p2 + 0xbaec0) };
                log_line(&format!(
                    "scene cached from render uniforms: scene={:#x} clock={}",
                    p2, clock
                ));
            }
        }
    }
    if !p11.is_null() && (p11 as usize) >= 0x10000 {
        let base = p11 as usize + 0x8f0; // +0x11e doubles
        let x = unsafe { read_f64_at(base) };
        let y = unsafe { read_f64_at(base + 8) };
        let z = unsafe { read_f64_at(base + 16) };
        let n = (x * x + y * y + z * z).sqrt();
        if n > 1.0e-6 && n < 1.0e9 {
            if let Ok(mut g) = LIGHT_DIR.lock() {
                g[0] = x;
                g[1] = y;
                g[2] = z;
            }
            LIGHT_DIR_VALID.store(true, Ordering::Relaxed);
        }
    }
}

static ORIG_FRAME_UNIFORMS: AtomicUsize = AtomicUsize::new(0);
/// Sun light direction captured from the frame state the renderer uploads as
/// sun_light_direction (uniform fill arg 11 + 0x11e doubles).
static LIGHT_DIR: Mutex<[f64; 3]> = Mutex::new([0.0; 3]);
static LIGHT_DIR_VALID: AtomicBool = AtomicBool::new(false);
static SUN_LOG: AtomicUsize = AtomicUsize::new(0);
static SUN_DIR: Mutex<[f64; 3]> = Mutex::new([0.0; 3]);
static SUN_DIR_VALID: AtomicBool = AtomicBool::new(false);
static SUN_PCT: AtomicUsize = AtomicUsize::new(0); // day percent x1000 from the sun hook

static SUN_EL: AtomicIsize = AtomicIsize::new(0); // deg x10, for the log
static SUN_AZ: AtomicIsize = AtomicIsize::new(0); // deg x10, for the log
/// Cache the scene object from the scene tick (its arg 1 IS the scene): the
/// day-cycle clock (+0xbaec0, copied from the authoritative +0xbbaac each
/// tick) and the sky state all live on it. Runs every game tick.
extern "C" fn ir_scene_tick_hook(p1: usize, p2: usize, p3: usize) {
    if p1 >= 0x10000 {
        SCENE.store(p1, Ordering::Relaxed);
        static FIRST: AtomicBool = AtomicBool::new(false);
        if !FIRST.swap(true, Ordering::Relaxed) {
            log_line("scene tick hook first call: scene object cached (sun live)");
        }
    }
    let original = ORIG_SCENE_TICK.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    let f: unsafe extern "C" fn(usize, usize, usize) =
        unsafe { std::mem::transmute(original) };
    unsafe { f(p1, p2, p3) }
}

/// Fires (burning objects) as IR emitters. Container layout is what
/// FUN_14030fd60 reads as param_5: ring slots +0x11d8, cap +0x11e0, start
/// +0x11e4, count +0x11e8; each group holds a world position at +0x38..+0x48
/// and an intensity float at +0x94. Treated as a ~1 m ~1000 C emitter scaled
/// by its intensity.
unsafe fn collect_fire_sources(
    c: usize,
    origin: &Vec3,
    range: f64,
    out: &mut Vec<IrSource>,
) {
    if c < 0x10000 {
        return;
    }
    let slots = read_usize_at(c + 0x11d8);
    let cap = read_u32_at(c + 0x11e0) as usize;
    let start = read_u32_at(c + 0x11e4) as usize;
    let count = read_u32_at(c + 0x11e8) as usize;
    if slots < 0x10000 || cap == 0 || count == 0 {
        return;
    }
    let (wx_fog, wx_rain) = weather_fog_rain();
    let n = count.min(256);
    for i in 0..n {
        let idx = (start + i) % cap;
        let group = read_usize_at(slots + idx * 8);
        if !(0x10000..0x7fff_ffff_ffff).contains(&group) {
            continue;
        }
        let x = read_f64_at(group + 0x38);
        let y = read_f64_at(group + 0x40);
        let z = read_f64_at(group + 0x48);
        let intensity = read_f32_at(group + 0x94) as f64;
        if !(x != 0.0 || y != 0.0 || z != 0.0) || intensity <= 0.0 {
            continue;
        }
        let dx = x - origin.0;
        let dy = y - origin.1;
        let dz = z - origin.2;
        let dist = (dx * dx + dy * dy + dz * dz).sqrt();
        let surface = (dist - 1.0).max(1.0);
        if surface > range {
            continue;
        }
        let flux = intensity * band_radiance(FIRE_T_K) / (surface * surface)
            / mwir_ref()
            * weather_transmittance(wx_fog, wx_rain, surface, false);
        out.push(IrSource {
            x,
            y,
            z,
            r_eff: 1.0,
            flux,
            az: 0.0,
            el: 0.0,
            theta: (1.0 / dist.max(0.5)).atan(),
            sky: false,
        });
    }
}

/// Diagnostic dump of the fires container (does a lit flare register here?).
/// Diagnostic dump of the fires container (logging builds only).
#[cfg(feature = "logging")]
unsafe fn log_fires_container(c: usize) {
    if c < 0x10000 {
        log_line("fires container: NULL");
        return;
    }
    let slots = read_usize_at(c + 0x11d8);
    let cap = read_u32_at(c + 0x11e0);
    let start = read_u32_at(c + 0x11e4);
    let count = read_u32_at(c + 0x11e8);
    log_line(&format!(
        "fires container {:#x}: slots={:#x} cap={} start={} count={}",
        c, slots, cap, start, count
    ));
    if slots < 0x10000 || cap == 0 || count == 0 {
        return;
    }
    let dump = (count as usize).min(4);
    for i in 0..dump {
        let idx = ((start as usize + i) % cap as usize) as usize;
        let group = read_usize_at(slots + idx * 8);
        if !(0x10000..0x7fff_ffff_ffff).contains(&group) {
            continue;
        }
        let x = read_f64_at(group + 0x38);
        let y = read_f64_at(group + 0x40);
        let z = read_f64_at(group + 0x48);
        let intensity = read_f32_at(group + 0x94);
        log_line(&format!(
            "  fire[{}] c=({:.1},{:.1},{:.1}) intensity={:.3}",
            idx, x, y, z, intensity
        ));
    }
}

extern "C" fn ir_scan(lua: *mut c_void) -> i32 {
    let lua = lua as usize;
    unsafe {
        let az_steer = lua_arg_number(lua, 1).unwrap_or(0.0);
        let el_steer = lua_arg_number(lua, 2).unwrap_or(0.0);
        let fov = lua_arg_number(lua, 3).unwrap_or(0.0833);
        let range = lua_arg_number(lua, 4).unwrap_or(200.0);
        let res_x = (lua_arg_number(lua, 5).unwrap_or(8.0) as i32).clamp(1, 64);
        let res_y = (lua_arg_number(lua, 6).unwrap_or(4.0) as i32).clamp(1, 64);
        // Arg 7 = IR gain: flux that maps to full-scale white.
        // Arg 8 = detect threshold (raw flux); the Lua applies it.
        let gain = lua_arg_number(lua, 7).unwrap_or(0.1);
        let _detect_flux = lua_arg_number(lua, 8).unwrap_or(0.01);
        let axis = lua_arg_number(lua, 9).unwrap_or(4.0) as i32;
        let occlude = lua_arg_number(lua, 10).unwrap_or(1.0) != 0.0;
        // arg 11: pack the image 2 cells per float (12-bit each) when the grid
        // is larger than one composite channel set (33..64 cells).
        let pack = lua_arg_number(lua, 11).unwrap_or(0.0) != 0.0;

        let cell_count = (res_x * res_y) as usize;
        let mut cells = vec![0.0f64; cell_count];
        let mut az_error = 0.0f64;
        let mut el_error = 0.0f64;
        let mut strength = 0.0f64;
        let mut total_flux = 0.0f64;
        let mut base_temp = 20.0f64;
        let mut source_count = 0usize;

        // Aiming diagnostics + cue state, hoisted for the throttled log.
        // Diagnostics-only mirror of the scan state (logging builds): the
        // throttled dump prints the boresight, steering and cone. diag_visible
        // is kept unconditionally — the out-of-cone cueing reads it.
        #[cfg(feature = "logging")]
        let mut diag_fwd = (0.0f64, 0.0f64, 0.0f64);
        // Filled from the pre-cone snapshot below; in player builds the
        // initial value is always overwritten before the first read, hence the
        // allow. The vector itself is read unconditionally by cueing.
        #[allow(unused_assignments)]
        let mut diag_visible: Vec<(f64, f64, f64, f64, f64)> = Vec::new();
        #[cfg(feature = "logging")]
        let mut diag_fov = 0.0f64;
        #[cfg(feature = "logging")]
        let mut diag_cone = 0.0f64;
        #[cfg(feature = "logging")]
        let mut diag_steer = (0.0f64, 0.0f64);
        let mut packed_vals: Vec<f64> = Vec::new();

        let component = lua_component(lua);
        #[allow(unused_assignments)]
        let mut origin = (0.0f64, 0.0f64, 0.0f64);
        // The scan runs whenever the component exists; without a temperature
        // evaluation yet (no engine/probe this session) heat sources and the
        // real ambient are unavailable, but fires/flares still are (fires
        // holder cached by the fire-render hook).
        if component != 0 {
            let matrix = read_component_matrix(component);
            origin = (matrix[12], matrix[13], matrix[14]);
            let (forward, up) = axes_for(axis, &matrix);
            let forward = normalize(forward);
            // right = up x fwd. With the game's handedness this is the seeker's
            // physical right for the +Y-forward mesh mount (verified in game:
            // the other order mirrors the array image left-right).
            let right = normalize(cross(up, forward));

            let fov_rad = fov * TAU;
            let az_center = az_steer * TAU;
            let el_center = el_steer * TAU;

            let mut weight_sum = 0.0f64;
            let mut weight_x = 0.0f64;
            let mut weight_y = 0.0f64;
            let az_sp = fov_rad / res_x as f64;
            let el_sp = fov_rad / res_y as f64;
            let cell_solid = az_sp * el_sp;

            // Collect every heat source and its emitted IR. This is radiometry
            // (emission ~ T^4 * area, received ~ 1/d^2), not the game's local
            // temperature field, so distant hot sources stay visible.
            let mut sources: Vec<IrSource> = Vec::new();
            let vcount = VARIANT_COUNT.load(Ordering::Relaxed).min(MAX_VARIANTS);
            let mut fires_holders = [0usize; MAX_VARIANTS + 1];
            let mut fires_n = 0usize;
            for i in 0..vcount {
                let a = variant_args(i);
                collect_ir_sources(a[0], &origin, range, &mut sources);
                let h = a[3];
                if h >= 0x10000 && !fires_holders[..fires_n].contains(&h) {
                    fires_holders[fires_n] = h;
                    fires_n += 1;
                }
            }
            let fh = FIRES_HOLDER.load(Ordering::Relaxed);
            if fh >= 0x10000 && !fires_holders[..fires_n].contains(&fh) {
                fires_holders[fires_n] = fh;
                fires_n += 1;
            }
            for &h in &fires_holders[..fires_n] {
                collect_fire_sources(h, &origin, range, &mut sources);
            }
            add_sun_source(origin, &mut sources);
            source_count = sources.len();
            base_temp = base_ambient_at(origin) as f64;
            for s in sources.iter_mut() {
                project_source(s, origin, forward, right, up);
            }

            // Line-of-sight: one physics ray per source. Blockers close to the
            // sensor (its own airframe) or to the emitter (its own hull edge)
            // are forgiven so enclosed emitters still glow.
            if occlude {
                let handle = lua_component_handle(lua);
                let ctx = if handle != 0 {
                    read_usize_at(handle + 8)
                } else {
                    0
                };
                let physics_ok = component != 0 && read_usize_at(component + 0x1d8) != 0;
                if physics_ok {
                    sources.retain(|s| {
                        let center_dist =
                            (origin.0 - s.x) * (origin.0 - s.x)
                                + (origin.1 - s.y) * (origin.1 - s.y)
                                + (origin.2 - s.z) * (origin.2 - s.z);
                        let center_dist = center_dist.sqrt();
                        match raycast_hit(ctx, origin, (s.x, s.y, s.z)) {
                            Some(hit_dist) => {
                                !(hit_dist > OCCLUDE_MIN_DIST
                                    && hit_dist
                                        < center_dist - s.r_eff - OCCLUDE_END_SLACK)
                            }
                            None => true,
                        }
                    });
                    source_count = sources.len();
                }
            }

            // Pre-cone snapshot for aiming diagnostics and cueing.
            #[cfg(feature = "logging")]
            {
                diag_fwd = forward;
                diag_fov = fov_rad;
                diag_cone = fov_rad * 0.5 + az_sp.max(el_sp);
                diag_steer = (az_center, el_center);
            }
            diag_visible = sources
                .iter()
                .map(|s| {
                    (
                        s.az,
                        s.el,
                        s.flux,
                        s.theta,
                        if s.sky { 1.0 } else { 0.0 },
                    )
                })
                .collect();

            // Acquisition cone: keep only sources the (steered) array can see,
            // and total the flux they deliver. The total — not the peak cell —
            // is what a real seeker detects, and it is conserved regardless of
            // FOV, so a narrower field costs no detection range.
            sources.retain(|s| {
                let d = s.az - az_center;
                let daz = d - TAU * (d / TAU).round();
                let del = s.el - el_center;
                let a = daz * s.el.cos().max(0.2);
                let inside = (a * a + del * del).sqrt()
                    < fov_rad * 0.5 + az_sp.max(el_sp) + s.theta;
                if inside {
                    total_flux += s.flux;
                }
                inside
            });

            // Cued acquisition: with an empty cone, report the direction to
            // the brightest source inside the forward hemisphere (~80 deg) as
            // the az/el error so the operator or a guidance loop can slew onto
            // it; the source only counts as detected once it enters the cone.
            let mut cue: Option<(f64, f64)> = None;
            let mut cue_blip = false; // sky sources steer, but never paint
            if sources.is_empty() {
                let mut best = 0.0f64;
                for s in &diag_visible {
                    let fx = s.1.cos() * s.0.cos();
                    if fx < 0.2 || s.2 <= best {
                        continue;
                    }
                    best = s.2;
                    cue = Some((s.0, s.1));
                    cue_blip = s.4 <= 0.5;
                }
            }

            // Rasterize. Unresolved sources (angular radius under one cell)
            // land on the nearest cell with a one-cell spill, so a sub-cell
            // target can never slip between cell centres. Resolved sources
            // dilute per cell (each pixel sees only part of them), which keeps
            // per-pixel brightness radiance-conserving.
            for s in &sources {
                let d = s.az - az_center;
                let daz = d - TAU * (d / TAU).round();
                let del = s.el - el_center;
                let col_f = (daz / fov_rad + 0.5) * res_x as f64 - 0.5;
                let row_f = (0.5 - del / fov_rad) * res_y as f64 - 0.5;
                let solid = std::f64::consts::PI * s.theta * s.theta;
                let dilut = if solid <= cell_solid {
                    1.0
                } else {
                    cell_solid / solid
                };
                for row in 0..res_y {
                    for col in 0..res_x {
                        let dc = col_f - col as f64;
                        let dr = row_f - row as f64;
                        let d_cells = (dc * dc + dr * dr).sqrt();
                        let w = if solid <= cell_solid {
                            (1.2 - d_cells).clamp(0.0, 1.0)
                        } else {
                            let a = dc * az_sp * s.el.cos().max(0.2);
                            let b = dr * el_sp;
                            let ang = (a * a + b * b).sqrt();
                            ((s.theta - ang) / (0.3 * s.theta)).clamp(0.0, 1.0)
                        };
                        if w <= 0.0 {
                            continue;
                        }
                        let idx = (row * res_x + col) as usize;
                        let nv = s.flux * dilut * w;
                        if nv > cells[idx] {
                            cells[idx] = nv;
                        }
                    }
                }
            }

            // Normalise the image and centroid from the final cell values.
            for value in cells.iter_mut() {
                let norm = if gain > 1e-9 { *value / gain } else { 0.0 };
                *value = norm.clamp(0.0, 1.0);
            }
            strength = if gain > 1e-9 {
                (total_flux / gain).clamp(0.0, 1.0)
            } else {
                0.0
            };
            for (index, value) in cells.iter().enumerate() {
                let col = (index as i32 % res_x) as f64;
                let row = (index as i32 / res_x) as f64;
                weight_sum += value;
                weight_x += value * (col + 0.5) / res_x as f64;
                weight_y += value * (0.5 - (row + 0.5) / res_y as f64);
            }

            if weight_sum > 1e-9 {
                az_error = (weight_x / weight_sum - 0.5) * fov;
                el_error = (weight_y / weight_sum) * fov;
            }

            // Paint the cue blip after normalisation so it stays a faint dot
            // and never contributes to the centroid above.
            if let Some((cue_az, cue_el)) = cue {
                let d = cue_az - az_center;
                let daz = d - TAU * (d / TAU).round();
                let del = cue_el - el_center;
                // The steering cue is always emitted — it is what a guidance
                // loop slews on — but only real (heat/fire) targets paint a
                // blip. A sky source (the sun) out of the FOV must not light
                // up a pixel on the array: the array shows the field of view,
                // not the whole sky.
                az_error = (daz / TAU).clamp(-0.5, 0.5);
                el_error = (del / TAU).clamp(-0.5, 0.5);
                if cue_blip {
                    let col_f = (daz / fov_rad + 0.5) * res_x as f64 - 0.5;
                    let row_f = (0.5 - del / fov_rad) * res_y as f64 - 0.5;
                    let col = (col_f.round() as i32).clamp(0, res_x - 1) as usize;
                    let row = (row_f.round() as i32).clamp(0, res_y - 1) as usize;
                    let idx = row * res_x as usize + col;
                    if cells[idx] < 0.08 {
                        cells[idx] = 0.08;
                    }
                }
            }

            // Optional packing for grids above one composite channel set
            // (33..64 cells): 2 cells per float, 12 bits each — value =
            // high*4096 + low. Exact in an f32 composite channel; the receiving
            // micro decodes with floor(v/4096) arithmetic. Resolution and the
            // pack flag are encoded on the composite's bool channels by the
            // component Lua so a monitor script can auto-decode.
            if pack {
                let n = (res_x * res_y) as usize;
                let mut i = 0usize;
                while i < n {
                    let hi = (cells[i] * 4095.0).floor();
                    let lo = if i + 1 < n {
                        (cells[i + 1] * 4095.0).floor()
                    } else {
                        0.0
                    };
                    packed_vals.push(hi * 4096.0 + lo);
                    i += 2;
                }
            }
        }

        // A scan counter for the throttled dump; in player builds nothing reads
        // it, so the binding is allow-listed rather than removed.
        #[allow(unused_variables)]
        let scan_index = SCAN_COUNT.fetch_add(1, Ordering::Relaxed);
        // Verbose diagnostics only exist in the logging build; the player build
        // compiles this whole block out, so a long session leaves a log with a
        // handful of lines instead of megabytes of scan dumps.
        #[cfg(feature = "logging")]
        if scan_index % 300 == 0 {
            {
                let ready = TEMP_READY.load(Ordering::SeqCst);
                log_line(&format!(
                    "scan: sources={} total_flux={:.5} base={} strength {:.3}",
                    source_count,
                    total_flux,
                    if ready {
                        format!("{:.1} C", base_temp)
                    } else {
                        "n/a".to_string()
                    },
                    strength
                ));
                log_line(&format!(
                    "scan boresight fwd=({:.3},{:.3},{:.3}) fov={:.1} deg steer=({:+.1},{:+.1}) deg cone=+-{:.1} deg",
                    diag_fwd.0,
                    diag_fwd.1,
                    diag_fwd.2,
                    diag_fov.to_degrees(),
                    diag_steer.0.to_degrees(),
                    diag_steer.1.to_degrees(),
                    diag_cone.to_degrees()
                ));
                for (i, s) in diag_visible.iter().enumerate().take(4) {
                    let d = s.0 - diag_steer.0;
                    let daz = d - TAU * (d / TAU).round();
                    let del = s.1 - diag_steer.1;
                    let off = ((daz * s.1.cos().max(0.2)).powi(2) + del * del).sqrt();
                    log_line(&format!(
                        "  dir[{}]{}: az={:+.2} deg el={:+.2} deg off={:.2} deg flux={:.4}",
                        i,
                        if s.4 > 0.5 { " SKY" } else { "" },
                        daz.to_degrees(),
                        del.to_degrees(),
                        off.to_degrees(),
                        s.2
                    ));
                }
                if !ready {
                    let fh = FIRES_HOLDER.load(Ordering::Relaxed);
                    if fh != 0 {
                        log_line(
                            "scan: temperature hook not fired yet; fires holder cached (flare/fire only mode)",
                        );
                    } else {
                        log_line("scan: temperature hook has not fired yet (TEMP_READY=false)");
                    }
                }
            }
            let sun_state = SUN_LOG.load(Ordering::Relaxed);
            {
                // Detailed sun diagnostics: the game's own rotating sun
                // function output, the light direction the renderer uploaded,
                // and the sky object's sun/moon vectors (scene+0x370+0x238).
                let scene = scene_env();
                if scene >= 0x10000 {
                    let settings_fn: unsafe extern "C" fn() -> usize = game_fn(RVA_SETTINGS);
                    let st = settings_fn();
                    if st >= 0x10000 {
                        let day_ticks = (read_u32_at(st) as u64) * 3600;
                        if day_ticks > 0 {
                            let clock = read_u32_at(scene + 0xbaec0) as u64;
                            let pct = (clock % day_ticks) as f64 / day_ticks as f64;
                            let sunrot: unsafe extern "C" fn(f32, *mut f32, *mut f32) =
                                game_fn(RVA_SUNROT);
                            let mut d = [0f32; 4];
                            let mut m = [0f32; 16];
                            sunrot(pct as f32, d.as_mut_ptr(), m.as_mut_ptr());
                            let (nx, ny, nz) = (-(d[0] as f64), -(d[1] as f64), -(d[2] as f64));
                            let nn = (nx * nx + ny * ny + nz * nz).sqrt().max(1.0e-9);
                            log_line(&format!(
                                "suncalc: scene={:#x} clock={} pct={:.4} hour={:.2} bf9e0=({:+.4},{:+.4},{:+.4}) to_sun el={:+.1} az={:+.1}",
                                scene,
                                clock,
                                pct,
                                pct * 24.0,
                                d[0],
                                d[1],
                                d[2],
                                (ny / nn).asin().to_degrees(),
                                (nz / nn).atan2(nx / nn).to_degrees()
                            ));
                        }
                    }
                    let sx = read_f32_at(scene + 0x5a8) as f64;
                    let sy = read_f32_at(scene + 0x5ac) as f64;
                    let sz = read_f32_at(scene + 0x5b0) as f64;
                    let mx = read_f32_at(scene + 0x5b4) as f64;
                    let my = read_f32_at(scene + 0x5b8) as f64;
                    let mz = read_f32_at(scene + 0x5bc) as f64;
                    let sn = (sx * sx + sy * sy + sz * sz).sqrt();
                    let mn = (mx * mx + my * my + mz * mz).sqrt();
                    log_line(&format!(
                        "sunsky: sun=({:+.4},{:+.4},{:+.4}) |{:.4}| el={:+.1} az={:+.1} | moon=({:+.4},{:+.4},{:+.4}) |{:.4}|",
                        sx, sy, sz, sn,
                        if sn > 1.0e-9 { (sy / sn).asin().to_degrees() } else { 0.0 },
                        if sn > 1.0e-9 { (sz / sn).atan2(sx / sn).to_degrees() } else { 0.0 },
                        mx, my, mz, mn
                    ));
                }
                if LIGHT_DIR_VALID.load(Ordering::Relaxed) {
                    let (lx, ly, lz) = {
                        let g = LIGHT_DIR.lock().unwrap_or_else(|e| e.into_inner());
                        (g[0], g[1], g[2])
                    };
                    let ln = (lx * lx + ly * ly + lz * lz).sqrt().max(1.0e-9);
                    log_line(&format!(
                        "sunlight: dir=({:+.4},{:+.4},{:+.4}) el={:+.1} az={:+.1}",
                        lx, ly, lz,
                        (ly / ln).asin().to_degrees(),
                        (lz / ln).atan2(lx / ln).to_degrees()
                    ));
                }
            }
            if sun_state == 0 {
                log_line("sun: no scene cached yet (waiting for the render uniforms hook)");
            } else if sun_state == 1 {
                log_line("sun: below the horizon");
            } else {
                let p = (sun_state - 2) as f64 / 1000.0;
                log_line(&format!(
                    "sun: day_percent={:.3} hour={:.1} emitter active el={:+.1} deg az={:+.1} deg{}",
                    p,
                    p * 24.0,
                    SUN_EL.load(Ordering::Relaxed) as f64 / 10.0,
                    SUN_AZ.load(Ordering::Relaxed) as f64 / 10.0,
                    "",
                ));
            }
            {
                let scene = SCENE.load(Ordering::Relaxed);
                let (fog, rain) = weather_fog_rain();
                if scene >= 0x10000 {
                    let w = scene + 0xbade8;
                    log_line(&format!(
                        "weather: fog={:.3} rain={:.3} (live {:.3}/{:.3}) wind={:.3} dir={:+.3} sigma={:.5}/m",
                        fog,
                        rain,
                        read_f32_at(w + 0x90),
                        read_f32_at(w + 0x98),
                        read_f32_at(w + 0xa0).max(read_f32_at(w + 0xa4)),
                        read_f32_at(w + 0xa8),
                        fog as f64 * FOG_SIGMA_PER_M + rain as f64 * RAIN_SIGMA_PER_M,
                    ));
                    log_line(&format!(
                        "weather: sun transmittance={:.5} (fog sigma {:.5}/m over {:.0} m)",
                        weather_transmittance(fog, rain, SUN_DIST, true),
                        FOG_SIGMA_SKY_PER_M,
                        SKY_PATH_CLAMP_M,
                    ));
                } else {
                    log_line("weather: scene not cached yet");
                }
            }
            log_line(&format!(
                "scan origin=({:.1},{:.1},{:.1})",
                origin.0, origin.1, origin.2
            ));
            let vcount = VARIANT_COUNT.load(Ordering::Relaxed).min(MAX_VARIANTS);
            log_line(&format!("scan: {vcount} heat container variant(s)"));
            for i in 0..vcount {
                let a = variant_args(i);
                log_line(&format!(
                    "  variant[{i}] heat={:#x} filter={:#x}",
                    a[0], a[7]
                ));
                log_heat_container(a[0]);
            }
            // Dump the fires holder the scan actually uses: the cached variant
            // when the temperature hook has run, the fire-render holder otherwise.
            let dump_holder = if vcount > 0 {
                variant_args(0)[3]
            } else {
                FIRES_HOLDER.load(Ordering::Relaxed)
            };
            log_fires_container(dump_holder);
        }

        let _ = lua_push_table(lua);
        if pack && !packed_vals.is_empty() {
            for (index, value) in packed_vals.iter().enumerate() {
                lua_push_number(lua, *value);
                lua_rawseti(lua, index as i64 + 1);
            }
        } else {
            for (index, value) in cells.iter().enumerate() {
                lua_push_number(lua, *value);
                lua_rawseti(lua, index as i64 + 1);
            }
        }
        lua_push_number(lua, az_error);
        lua_push_number(lua, el_error);
        lua_push_number(lua, strength);
        lua_push_number(lua, total_flux);
        lua_push_number(lua, base_temp);
        lua_push_number(lua, source_count as f64);
    }
    7
}

// ---------------------------------------------------------------------------
// vector / matrix helpers
// ---------------------------------------------------------------------------

type Vec3 = (f64, f64, f64);

fn cross(a: Vec3, b: Vec3) -> Vec3 {
    (
        a.1 * b.2 - a.2 * b.1,
        a.2 * b.0 - a.0 * b.2,
        a.0 * b.1 - a.1 * b.0,
    )
}

fn normalize(v: Vec3) -> Vec3 {
    let length = (v.0 * v.0 + v.1 * v.1 + v.2 * v.2).sqrt();
    if length < 1e-12 {
        (0.0, 0.0, 0.0)
    } else {
        (v.0 / length, v.1 / length, v.2 / length)
    }
}

/// Reads the component's world transform. The 4x4 matrix is stored
/// column-major at `component + 0x1e8` (verified against `FUN_140339aa0`).
unsafe fn read_component_matrix(component: usize) -> [f64; 16] {
    let base = (component + COMPONENT_MATRIX_OFFSET) as *const f64;
    let mut matrix = [0.0f64; 16];
    for (index, slot) in matrix.iter_mut().enumerate() {
        *slot = *base.add(index);
    }
    matrix
}

unsafe fn axes_for(axis: i32, matrix: &[f64; 16]) -> (Vec3, Vec3) {
    let column = |c: usize| (matrix[c * 4], matrix[c * 4 + 1], matrix[c * 4 + 2]);
    let negate = |v: Vec3| (-v.0, -v.1, -v.2);
    let x = column(0);
    let y = column(1);
    let z = column(2);
    match axis {
        0 => (x, y),
        1 => (negate(x), y),
        2 => (y, negate(z)),
        3 => (negate(y), negate(z)),
        4 => (z, y),
        5 => (negate(z), y),
        _ => (z, y),
    }
}

// ---------------------------------------------------------------------------
// hook installation
// ---------------------------------------------------------------------------

unsafe fn install_hooks() -> Result<(), String> {
    let base = MODULE_BASE.load(Ordering::SeqCst);
    if base == 0 {
        return Err("module base is not resolved".to_string());
    }

    let registrar = base + RVA_COMPONENT_LUA_REGISTRAR;
    if !bytes_match(registrar, &REGISTRAR_PROLOGUE) {
        return Err(format!(
            "component Lua registrar prologue mismatch at {registrar:#x}; this build is not {GAME_BUILD_LABEL}"
        ));
    }
    let temperature = base + RVA_TEMPERATURE;
    if !bytes_match(temperature, &TEMPERATURE_PROLOGUE) {
        return Err(format!(
            "temperature function prologue mismatch at {temperature:#x}; this build is not {GAME_BUILD_LABEL}"
        ));
    }
    let fires_build = base + RVA_FIRES_BUILD;
    if !bytes_match(fires_build, &FIRES_PROLOGUE) {
        return Err(format!(
            "fire vertex builder prologue mismatch at {fires_build:#x}; this build is not {GAME_BUILD_LABEL}"
        ));
    }
    let scene_tick = base + RVA_SCENE_TICK;
    if !bytes_match(scene_tick, &SCENE_TICK_PROLOGUE) {
        return Err(format!(
            "scene tick prologue mismatch at {scene_tick:#x}; this build is not {GAME_BUILD_LABEL}"
        ));
    }
    let sun_update = base + RVA_SUN_UPDATE;
    if !bytes_match(sun_update, &SUN_UPDATE_PROLOGUE) {
        return Err(format!(
            "sun update prologue mismatch at {sun_update:#x}; this build is not {GAME_BUILD_LABEL}"
        ));
    }
    let frame_uniforms = base + RVA_FRAME_UNIFORMS;
    if !bytes_match(frame_uniforms, &FRAME_UNIFORMS_PROLOGUE) {
        return Err(format!(
            "frame uniforms prologue mismatch at {frame_uniforms:#x}; this build is not {GAME_BUILD_LABEL}"
        ));
    }

    let original_registrar = MinHook::create_hook(
        registrar as *mut c_void,
        ir_registrar_hook as *const c_void as *mut c_void,
    )
    .map_err(|status| format!("MinHook create_hook(registrar) failed: {status}"))?;
    ORIG_REGISTRAR.store(original_registrar as usize, Ordering::SeqCst);
    MinHook::enable_hook(registrar as *mut c_void)
        .map_err(|status| format!("MinHook enable_hook(registrar) failed: {status}"))?;

    let original_temperature = MinHook::create_hook(
        temperature as *mut c_void,
        ir_temperature_hook as *const c_void as *mut c_void,
    )
    .map_err(|status| format!("MinHook create_hook(temperature) failed: {status}"))?;
    ORIG_TEMPERATURE.store(original_temperature as usize, Ordering::SeqCst);
    MinHook::enable_hook(temperature as *mut c_void)
        .map_err(|status| format!("MinHook enable_hook(temperature) failed: {status}"))?;

    let original_fires = MinHook::create_hook(
        fires_build as *mut c_void,
        ir_fires_hook as *const c_void as *mut c_void,
    )
    .map_err(|status| format!("MinHook create_hook(fires builder) failed: {status}"))?;
    ORIG_FIRES_BUILD.store(original_fires as usize, Ordering::SeqCst);
    MinHook::enable_hook(fires_build as *mut c_void)
        .map_err(|status| format!("MinHook enable_hook(fires builder) failed: {status}"))?;

    let original_scene_tick = MinHook::create_hook(
        scene_tick as *mut c_void,
        ir_scene_tick_hook as *const c_void as *mut c_void,
    )
    .map_err(|status| format!("MinHook create_hook(scene tick) failed: {status}"))?;
    ORIG_SCENE_TICK.store(original_scene_tick as usize, Ordering::SeqCst);
    MinHook::enable_hook(scene_tick as *mut c_void)
        .map_err(|status| format!("MinHook enable_hook(scene tick) failed: {status}"))?;

    let original_sun_update = MinHook::create_hook(
        sun_update as *mut c_void,
        ir_sun_update_hook as *const c_void as *mut c_void,
    )
    .map_err(|status| format!("MinHook create_hook(sun update) failed: {status}"))?;
    ORIG_SUN_UPDATE.store(original_sun_update as usize, Ordering::SeqCst);
    MinHook::enable_hook(sun_update as *mut c_void)
        .map_err(|status| format!("MinHook enable_hook(sun update) failed: {status}"))?;

    let original_frame_uniforms = MinHook::create_hook(
        frame_uniforms as *mut c_void,
        ir_frame_uniforms_hook as *const c_void as *mut c_void,
    )
    .map_err(|status| format!("MinHook create_hook(frame uniforms) failed: {status}"))?;
    ORIG_FRAME_UNIFORMS.store(original_frame_uniforms as usize, Ordering::SeqCst);
    MinHook::enable_hook(frame_uniforms as *mut c_void)
        .map_err(|status| format!("MinHook enable_hook(frame uniforms) failed: {status}"))?;

    HOOKS_INSTALLED.store(true, Ordering::SeqCst);
    Ok(())
}

// ---------------------------------------------------------------------------
// bootstrap
// ---------------------------------------------------------------------------

fn bootstrap() {
    let Some(dll_path) = self_module_path() else {
        return;
    };
    let plugin_dir = dll_path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let log_dir = plugin_dir.join("logs");
    let _ = fs::create_dir_all(&log_dir);
    set_log_path(log_dir.join("infrared_bootstrap.log"));

    log_line(&format!(
        "infrared bootstrap start build={GAME_BUILD_LABEL} dll={}",
        dll_path.display()
    ));

    let base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
    if base == 0 {
        log_line("could not resolve the main module base");
        return;
    }
    MODULE_BASE.store(base, Ordering::SeqCst);
    log_line(&format!("game module base = {base:#x}"));

    match unsafe { install_hooks() } {
        Ok(()) => log_line("hooks installed (component Lua registrar + temperature)"),
        Err(error) => {
            log_line(&format!("hook installation failed: {error}"));
            return;
        }
    }

    // Keep the worker alive so the log stays writable and the DLL is not
    // considered idle; also report registrar activity periodically.
    for _ in 0..600 {
        thread::sleep(Duration::from_secs(1));
        if REGISTRAR_HITS.load(Ordering::Relaxed) > 0 {
            log_line(&format!(
                "ir table registered {} time(s)",
                REGISTRAR_HITS.load(Ordering::Relaxed)
            ));
            break;
        }
    }
}

#[no_mangle]
pub extern "system" fn DllMain(module: HINSTANCE, reason: u32, _reserved: *mut c_void) -> i32 {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason == DLL_PROCESS_ATTACH {
        unsafe {
            let _ = DisableThreadLibraryCalls(module);
        }
        SELF_MODULE.store(module as usize, Ordering::SeqCst);
        let spawned = thread::Builder::new()
            .name("stormworks-infrared-bootstrap".to_string())
            .spawn(|| {
                thread::sleep(Duration::from_millis(500));
                bootstrap();
            });
        if spawned.is_err() {
            // Nothing useful to do here; the DLL stays loaded and silently
            // does nothing.
        }
    }
    1
}
