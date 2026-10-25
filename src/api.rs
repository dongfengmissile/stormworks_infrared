//! The Lua surface the components call: `ir.probe` and `ir.scan` in the
//! component-mod state, `ir.frame` / `ir.flir` / `ir.flir_config` in the microcontroller
//! (screen) state, plus the table registration the two initializers invoke.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::heat::{base_ambient_at, temperature_at, variant_args};
#[cfg(feature = "logging")]
use crate::heat::{log_fires_container, log_heat_container, log_volume_heat_sources};
use crate::ir::{
    axes_for, cross, normalize, project_source, raycast_hit, read_component_matrix, IrSource,
};
#[cfg(feature = "logging")]
use crate::ir::{weather_fog_rain, weather_transmittance};
use crate::logging::log_line;
use crate::lua::*;
use crate::memory::{game_fn, in_game_image, read_usize_at, readable};
#[cfg(feature = "logging")]
use crate::memory::{read_f32_at, read_u32_at};
use crate::offsets::*;
use crate::sources::{
    collect_emitter_sources, collect_fire_sources, collect_ir_sources, collect_volume_heat_sources,
};
use crate::state::*;
use crate::sun::add_sun_source;
#[cfg(feature = "logging")]
use crate::sun::scene_env;

/// Register the `ir` global into the component-mod Lua state, exactly the way
/// the game registers its own `matrix` / `parser` / `component` tables.
pub(crate) unsafe fn register_ir_table(context: usize) {
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

/// Register the `ir` table into the drawing (screen) Lua environment.
///
/// `ir.frame` reads the image a seeker published, `ir.probe` samples the heat
/// model, and `ir.flir_config` resolves this microcontroller's video link to
/// the camera it feeds - none of them needs a component-handle upvalue.
///
/// `ir.scan` is deliberately absent: it is a seeker API (a component with a
/// world matrix and a physics context for occlusion), and in this state the
/// game's upvalues are per-table script objects (`screen`, `async`, ...), not
/// component handles (RE_NOTES §1.7.1). Registering it here previously handed
/// `lua_component` the address of this function's own local, which is neither a
/// component nor valid after the initializer returned.
///
/// The upvalue is therefore NULL, the same value the game hands its own
/// `debug` table, so `lua_component` / `lua_component_handle` return 0 instead
/// of reading through a dangling pointer. A microcontroller call that needs its
/// component derives it from the `L-8` back-pointer instead (`mc_component`).
pub(crate) unsafe fn register_ir_screen_table(state: usize) {
    if state == 0 {
        return;
    }
    let alloc: unsafe extern "C" fn(*mut usize, u32) = game_fn(RVA_STRING_ALLOC);
    let fill: unsafe extern "C" fn(usize, *const u8, u32) = game_fn(RVA_STRING_FILL);
    let register: unsafe extern "C" fn(usize, *const usize, *const FnPair, usize) =
        game_fn(RVA_REGISTER_TABLE);

    let mut name_object: usize = 0;
    alloc(&mut name_object, 3);
    if name_object == 0 {
        log_line("ir screen registration: name allocation failed");
        return;
    }
    fill(name_object, b"ir\0".as_ptr(), 3);

    let pairs = [
        FnPair {
            name: b"frame\0".as_ptr(),
            func: ir_frame as *const () as usize,
        },
        FnPair {
            name: b"probe\0".as_ptr(),
            func: ir_probe as *const () as usize,
        },
        FnPair {
            name: b"flir_config\0".as_ptr(),
            func: ir_flir_config as *const () as usize,
        },
        FnPair {
            name: b"flir\0".as_ptr(),
            func: ir_flir as *const () as usize,
        },
        FnPair {
            name: std::ptr::null(),
            func: 0,
        },
    ];

    // The fourth argument is the lightuserdata the game installs as the one
    // upvalue of every registered function. NULL is what the game itself hands
    // its `debug` table, and nothing above dereferences it.
    register(state, &name_object as *const usize, pairs.as_ptr(), 0);
}

// ---------------------------------------------------------------------------
// microcontroller component lookup (RE_NOTES §1.7.1-1.7.3)
// ---------------------------------------------------------------------------

/// `c_scene_vehicle_component_camera` vtable.
const CAMERA_VTABLE_RVA: usize = 0x00b5_3420;
/// `c_vehicle_logic_slot_output_video::vftable` - a camera's video output slot.
const SLOT_OUTPUT_VIDEO_VTABLE_RVA: usize = 0x00b7_46f8;
/// The microcontroller's embedded `c_microprocessor_lua_script`.
const MC_SCRIPT_OFFSET: usize = 0x660;
/// `node+0x28` / `node+0x30` of the MC's video input node (`comp+0x110`): the
/// source slot and source node the game's per-tick link resolve writes.
/// A camera's `c_vehicle_logic_slot_output_video` (camera + 0x190).
const CAMERA_VIDEO_SLOT: usize = 0x190;

/// Why a microcontroller call could not resolve a component or a camera, for
/// the one-shot log line.
const NO_MC_STATE: &str =
    "not called from a microcontroller Lua state (no L-8 script back-pointer)";
const NO_CAMERA: &str = "this microcontroller's video input is not fed by a camera video output";

/// `c_microprocessor_lua_script` -> owning microcontroller component.
///
/// The game's screen init writes the script object to `*(u64*)(L-8)`
/// (`0x2E5261`), and the script object is embedded in the component at
/// `+0x660`, so `component = *(u64*)(L-8) - 0x660`. Both hops are validated
/// (readable, and a class vtable inside the game image) before the component is
/// returned, because a Lua state from the component-mod side has no such
/// back-pointer.
unsafe fn mc_component(lua: usize) -> Option<usize> {
    if lua < 0x10000 || !readable(lua - 8, 8) {
        return None;
    }
    let script = read_usize_at(lua - 8);
    if !readable(script, 8) || !in_game_image(read_usize_at(script)) {
        return None;
    }
    let base = MODULE_BASE.load(Ordering::SeqCst);
    if base == 0 || read_usize_at(script) != base + LUA_SCRIPT_VTABLE_RVA {
        return None;
    }
    let component = script.wrapping_sub(MC_SCRIPT_OFFSET);
    if rd(component)? != base + LUA_BLOCK_VTABLE_RVA {
        return None;
    }
    Some(component)
}

/// Vtable RVAs of the MC graph objects (RE_NOTES §1.7.3).
const LUA_SCRIPT_VTABLE_RVA: usize = 0x00b6_f468; // c_microprocessor_lua_script
const LUA_BLOCK_VTABLE_RVA: usize = 0x00b9_a710; // c_microprocessor_component_lua_script
const OUT_VIDEO_NODE_VTABLE_RVA: usize = 0x00af_72c8; // ..._logic_node_output_video
const BRIDGE_VIDEO_IN_VTABLE_RVA: usize = 0x00b0_72d8; // ..._bridge_video_input
const VIDEO_SWITCHBOX_VTABLE_RVA: usize = 0x00b8_2ee8; // ..._video_switchbox
/// Input node fields: source slot / source (graph) output node.
const NODE_SRC_NODE: usize = 0x30;
/// bridge_video_input's in-node (+0x50); switchbox in-nodes at +0x50 and +0x2A8.
const SWITCHBOX_IN_NODES: [usize; 2] = [0x50, 0x2a8];
/// Switchbox's cached bool switch value (bool in-node +0x500, value +0x38).
const SWITCHBOX_STATE: usize = 0x538;
const MC_VIDEO_IN_NODE: usize = 0x110;
const MC_HOP_CAP: usize = 8;

/// Guarded 8-aligned read.
unsafe fn rd(p: usize) -> Option<usize> {
    if p < 0x10000 || p & 7 != 0 || !readable(p, 8) {
        return None;
    }
    Some(unsafe { read_usize_at(p) })
}

const MC_GROUP: usize = 0x9a8;
const GROUP_VTABLE_RVA: usize = 0x00b4_6bd8;
const GROUP_IN_MC: usize = 0x1c0;
const MC_COMP_VTABLE_RVA: usize = 0x00b8_4dc8;
const MC_BODY: usize = 0x28;
const MC_INPUT_SLOT_VTABLE_RVA: usize = 0x00b7_e7e8; // c_vehicle_logic_slot_input_video
const BRIDGE_INSLOT: usize = 0x78;
const BODY_COMPONENTS: usize = 0x3f0; // ring: ptr, +8 cap, +0xC off, +0x10 n
const BODY_VEHICLE: usize = 0x250;
const VEHICLE_BODIES: usize = 0x50; // ring of 0x30-byte entries, body* at +0x18
const SLOT_CONSUMERS: usize = 0x238; // ring: ptr, +8 cap, +0xC off, +0x10 n
const RING_CAP_MAX: usize = 4096;
const SCAN_BUDGET: usize = 20000;

/// Diagnostics of one MC camera resolve.
#[derive(Default, Debug)]
struct McTrace {
    group_vt: Option<usize>,
    mc: usize,
    mc_vt: Option<usize>,
    inslot: usize,
    inslot_vt: Option<usize>,
    body: usize,
    vehicle: usize,
    bodies: usize,
    cameras: usize,
    consumer_counts: Vec<usize>,
    hops: Vec<usize>,
    result: &'static str,
}

/// Validated ring header at `p`: (ptr, cap, off, n).
unsafe fn ring(p: usize) -> Option<(usize, usize, usize, usize)> {
    unsafe {
        let ptr = rd(p)?;
        let cap = rd(p + 8)? & 0xffff_ffff;
        let off = (rd(p + 8)? >> 32) & 0xffff_ffff;
        let n = rd(p + 0x10)? & 0xffff_ffff;
        if ptr == 0 || cap == 0 || cap > RING_CAP_MAX || n > cap {
            return None;
        }
        Some((ptr, cap, off, n))
    }
}

/// Find the MC's own input port slot by following the input node chain.
unsafe fn mc_inslot(base: usize, lua_blk: usize, t: &mut McTrace) -> Option<usize> {
    let mut in_node = lua_blk + MC_VIDEO_IN_NODE;
    for _ in 0..MC_HOP_CAP {
        let node = unsafe { rd(in_node + NODE_SRC_NODE)? };
        let vt = unsafe { rd(node)? };
        t.hops.push(vt.wrapping_sub(base));
        if vt != base + OUT_VIDEO_NODE_VTABLE_RVA {
            return None;
        }
        let owner = unsafe { rd(node + 8)? };
        let ovt = unsafe { rd(owner)? };
        t.hops.push(ovt.wrapping_sub(base));
        if ovt == base + BRIDGE_VIDEO_IN_VTABLE_RVA {
            let slot = unsafe { rd(owner + BRIDGE_INSLOT)? };
            t.inslot = slot;
            t.inslot_vt = unsafe { rd(slot) };
            return (t.inslot_vt == Some(base + MC_INPUT_SLOT_VTABLE_RVA)).then_some(slot);
        } else if ovt == base + VIDEO_SWITCHBOX_VTABLE_RVA {
            // Follow the input the switch currently forwards (0x140389DA0:
            // state byte != 0 -> +0x50, else +0x2A8).
            if !readable(owner + SWITCHBOX_STATE, 1) {
                return None;
            }
            let on = unsafe { *((owner + SWITCHBOX_STATE) as *const u8) } != 0;
            in_node = owner + SWITCHBOX_IN_NODES[if on { 0 } else { 1 }];
        } else {
            return None;
        }
    }
    None
}

/// Scan one body's components; push cameras into `cams`. Returns the camera
/// whose consumer ring contains `inslot`.
unsafe fn scan_body(
    base: usize,
    body: usize,
    inslot: usize,
    cams: &mut Vec<usize>,
    budget: &mut usize,
    t: &mut McTrace,
) -> Option<usize> {
    unsafe {
        let (arr, cap, off, n) = ring(body + BODY_COMPONENTS)?;
        let mut found = None;
        for i in 0..n {
            if *budget == 0 {
                break;
            }
            *budget -= 1;
            let Some(c) = rd(arr + 8 * ((i + off) % cap)) else { continue };
            if c == 0 || rd(c) != Some(base + CAMERA_VTABLE_RVA) || cams.contains(&c) {
                continue;
            }
            cams.push(c);
            t.cameras += 1;
            let Some((cp, cc, co, cn)) = ring(c + CAMERA_VIDEO_SLOT + SLOT_CONSUMERS) else {
                t.consumer_counts.push(usize::MAX);
                continue;
            };
            t.consumer_counts.push(cn);
            if inslot != 0 && found.is_none() {
                for j in 0..cn {
                    if *budget == 0 {
                        break;
                    }
                    *budget -= 1;
                    if rd(cp + 8 * ((j + co) % cc)) == Some(inslot) {
                        found = Some(c);
                        break;
                    }
                }
            }
        }
        found
    }
}

unsafe fn mc_resolve(lua_blk: usize, t: &mut McTrace) -> Option<usize> {
    let base = MODULE_BASE.load(Ordering::SeqCst);
    if base == 0 {
        return None;
    }
    unsafe {
        let group = rd(lua_blk + MC_GROUP)?;
        t.group_vt = rd(group);
        if t.group_vt != Some(base + GROUP_VTABLE_RVA) {
            t.result = "group vt";
            return None;
        }
        t.mc = group.wrapping_sub(GROUP_IN_MC);
        t.mc_vt = rd(t.mc);
        if t.mc_vt != Some(base + MC_COMP_VTABLE_RVA) {
            t.result = "mc vt";
            return None;
        }
        let inslot = mc_inslot(base, lua_blk, t).unwrap_or(0);
        t.body = rd(t.mc + MC_BODY)?;
        if t.body == 0 {
            t.result = "no body";
            return None;
        }
        let mut cams = Vec::new();
        let mut budget = SCAN_BUDGET;
        if let Some(c) = scan_body(base, t.body, inslot, &mut cams, &mut budget, t) {
            t.result = "consumer match (body)";
            return Some(c);
        }
        t.vehicle = rd(t.body + BODY_VEHICLE).unwrap_or(0);
        if t.vehicle != 0 {
            // Other bodies (subgrids) of the vehicle: the body-pointer set at
            // vehicle+0 that blocktemp walks every tick, then the older 0x30-byte
            // ring at +0x50. A camera on a different subgrid than the MC was
            // never found through the ring alone.
            let mut others: Vec<usize> = Vec::new();
            if let Some(set) = crate::blocktemp::read_set(t.vehicle, 256, 8, crate::blocktemp::MAX_SET_MOD) {
                others.extend((0..set.count()).map(|j| set.item(j)));
            }
            if let Some((arr, cap, off, n)) = ring(t.vehicle + VEHICLE_BODIES) {
                others.extend((0..n).filter_map(|i| rd(arr + 0x30 * ((i + off) % cap) + 0x18)));
            }
            for b in others {
                if b == 0 || b == t.body || rd(b + BODY_VEHICLE) != Some(t.vehicle) {
                    continue;
                }
                t.bodies += 1;
                if let Some(c) = scan_body(base, b, inslot, &mut cams, &mut budget, t) {
                    t.result = "consumer match (vehicle)";
                    return Some(c);
                }
            }
        }
        if cams.len() == 1 {
            t.result = "sole camera fallback";
            return Some(cams[0]);
        }
        t.result = "no match / ambiguous";
        None
    }
}

/// The camera whose video output feeds the MC Lua block `component` (the
/// `lua_blk` from `mc_component`), delivered as `(camera, 0)`.
unsafe fn mc_camera(component: usize) -> Option<(usize, usize)> {
    let base = MODULE_BASE.load(Ordering::SeqCst);
    if base == 0 {
        return None;
    }
    unsafe {
        if rd(component)? != base + LUA_BLOCK_VTABLE_RVA {
            return None;
        }
        // The full resolve scans every component of the vehicle (two per tick per
        // script: ir.flir + ir.flir_config). Reuse the last answer while the
        // cheap last hop still holds: the camera keeps its vtable and its
        // consumer ring still contains this MC's input slot (re-derived from the
        // live node chain). A sole-camera fallback (no slot) is re-scanned
        // every second.
        let now = crate::state::now_ms();
        let mut t = McTrace::default();
        let inslot = mc_inslot(base, component, &mut t).unwrap_or(0);
        let cached = MC_CAMERA_CACHE
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&component)
            .copied();
        if let Some((camera, slot, at_ms)) = cached {
            let fresh = if slot != 0 {
                slot == inslot && camera_feeds(base, camera, slot)
            } else {
                inslot == 0
                    && now.saturating_sub(at_ms) < 1000
                    && rd(camera) == Some(base + CAMERA_VTABLE_RVA)
            };
            if fresh {
                return Some((camera, 0));
            }
        }
        let camera = mc_resolve(component, &mut McTrace::default())?;
        let mut cache = MC_CAMERA_CACHE.lock().unwrap_or_else(|p| p.into_inner());
        if cache.len() >= 256 {
            cache.clear();
        }
        cache.insert(component, (camera, inslot, now));
        Some((camera, 0))
    }
}

/// lua block -> (camera, input slot it was matched through, resolve time ms).
static MC_CAMERA_CACHE: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<usize, (usize, usize, u64)>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// `camera` is still a camera and its video output's consumer ring holds `slot`.
unsafe fn camera_feeds(base: usize, camera: usize, slot: usize) -> bool {
    unsafe {
        if rd(camera) != Some(base + CAMERA_VTABLE_RVA) {
            return false;
        }
        let Some((cp, cc, co, cn)) = ring(camera + CAMERA_VIDEO_SLOT + SLOT_CONSUMERS) else {
            return false;
        };
        (0..cn.min(64)).any(|j| rd(cp + 8 * ((j + co) % cc)) == Some(slot))
    }
}

/// One-shot (logging builds) diagnostics for a failed camera resolve.
#[cfg(feature = "logging")]
unsafe fn log_mc_failure(lua: usize) {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if LOGGED.swap(true, Ordering::Relaxed) {
        return;
    }
    let base = MODULE_BASE.load(Ordering::SeqCst);
    unsafe {
        let script = rd(lua.wrapping_sub(8)).unwrap_or(0);
        let lua_blk = script.wrapping_sub(MC_SCRIPT_OFFSET);
        let mut t = McTrace::default();
        let r = mc_resolve(lua_blk, &mut t);
        let f = |v: Option<usize>| v.map_or("unreadable".to_string(), |v| format!("{:#x}", v.wrapping_sub(base)));
        log_line(&format!(
            "mc camera resolve failed: lua_blk={lua_blk:#x} result={:?} ({}) group_vt={} (want {GROUP_VTABLE_RVA:#x}) \
mc={:#x} vt={} (want {MC_COMP_VTABLE_RVA:#x}) inslot={:#x} vt={} (want {MC_INPUT_SLOT_VTABLE_RVA:#x}) \
body={:#x} vehicle={:#x} other_bodies={} cameras_scanned={} consumer_counts={:?} hop_vtable_rvas={:#x?} payload_count={:?}",
            r.map(|c| c - base),
            t.result,
            f(t.group_vt),
            t.mc,
            f(t.mc_vt),
            t.inslot,
            f(t.inslot_vt),
            t.body,
            t.vehicle,
            t.bodies,
            t.cameras,
            t.consumer_counts,
            t.hops,
            readable(lua_blk + 0x360, 4).then(|| read_u32_at(lua_blk + 0x360)),
        ));
    }
}
#[cfg(not(feature = "logging"))]
unsafe fn log_mc_failure(_lua: usize) {}

// ---------------------------------------------------------------------------
// injected Lua functions
// ---------------------------------------------------------------------------

pub(crate) extern "C" fn ir_probe(lua: *mut c_void) -> i32 {
    // A panic must never unwind through the Lua C function: the game aborts.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { probe_inner(lua) }))
        .unwrap_or(0)
}

unsafe fn probe_inner(lua: *mut c_void) -> i32 {
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

/// A seeker's last rendered image, for the drawing Lua environment.
///
/// Takes the seeker's id (its `ir.scan` return value, which the seeker
/// publishes to the microcontroller over the array composite), so a vehicle -
/// or a world - with several seekers draws exactly the one it is wired to.
///
/// Returns (cells, width, height), cells holding one 0..255 value per cell in
/// row-major order. This exists because a logic composite is 32 floats: packed
/// two cells per float that caps an image at 64 cells, so a 64x64 frame cannot
/// reach a monitor that way. The drawing environment has no component context
/// either, so it cannot run the scan itself - it reads what the seeker
/// published instead.
pub(crate) extern "C" fn ir_frame(lua: *mut c_void) -> i32 {
    // A panic must never unwind through the Lua C function: the game aborts.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { frame_inner(lua) }))
        .unwrap_or(0)
}

unsafe fn frame_inner(lua: *mut c_void) -> i32 {
    let lua = lua as usize;
    unsafe {
        // arg 1: which seeker's frame. 0 (or omitted) returns nothing, because
        // a seeker's id is never 0 - the drawing script passes what it read from
        // the composite, so it asks for its own seeker and no other.
        let id = (lua_arg_number(lua, 1).unwrap_or(0.0) as i64)
            .clamp(0, FRAME_ID_MAX as i64) as u32;
        let (width, height, bytes) = {
            let frames = LAST_FRAME
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match frames.get(&id) {
                Some((width, height, bytes)) => (*width, *height, bytes.clone()),
                None => (0, 0, Vec::new()),
            }
        };
        lua_push_table(lua);
        for (index, value) in bytes.iter().enumerate() {
            lua_push_number(lua, *value as f64);
            lua_rawseti(lua, index as i64 + 1);
        }
        lua_push_number(lua, width as f64);
        lua_push_number(lua, height as f64);
    }
    3
}

/// A seeker's identity for the frame map: 24 bits derived from its component.
///
/// Live components have distinct pointers, so this is unique among the seekers
/// actually running, without anyone having to number them - which is the point:
/// a channel number is global to the whole game process, so two vehicles that
/// both picked 1 would overwrite each other's image. It changes when a vehicle
/// is respawned, which the drawing side simply re-reads.
pub(crate) fn seeker_id(component: usize) -> u32 {
    if component == 0 {
        return 0;
    }
    // Mix the whole pointer down to 24 bits rather than truncating it, so two
    // components that differ only in their high bits still separate, and the
    // value does not read as a raw address. Deterministic for a given component.
    let mut mixed = (component as u64) ^ 0x9e37_79b9_7f4a_7c15;
    mixed = mixed.wrapping_mul(0xff51_afd7_ed55_8ccd);
    mixed ^= mixed >> 29;
    let id = (mixed & FRAME_ID_MAX as u64) as u32;
    if id == 0 {
        1
    } else {
        id
    }
}

// ---------------------------------------------------------------------------
// FLIR camera configuration (`ir.flir_config`, microcontroller state only)
// ---------------------------------------------------------------------------

/// Narrowest display window accepted. A zero (or near-zero) span would divide
/// the shader by ~0 instead of mapping temperatures, so it is rejected as an
/// invalid argument rather than producing an unusable image.
const FLIR_MIN_WINDOW_C: f32 = 0.01;
/// Cap on configured cameras. Keys are component pointers and a long session
/// spawns cameras repeatedly, so the map is bounded; on overflow it is cleared
/// and every live script republishes its config on its next tick.
const FLIR_CONFIG_MAX: usize = 256;

/// `ir.flir_config(r, g, b, black_hot, tmin, tmax)` - microcontroller only.
///
/// Sets the FLIR presentation of the camera whose video output feeds this
/// microcontroller's video input, keyed by the camera component pointer - the
/// key `flir.rs` reads from `FLIR_CONFIG` for the view it is compositing.
/// Returns true when the link resolved and the config was stored, false (and a
/// one-shot log line) otherwise. The link fields are only valid while the
/// game's tick owns them, which is exactly when a script call runs, so they are
/// read here and never cached.
pub(crate) extern "C" fn ir_flir_config(lua: *mut c_void) -> i32 {
    // A panic must never unwind through the Lua C function: the game aborts.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { flir_config_inner(lua) }))
        .unwrap_or(0)
}

unsafe fn flir_config_inner(lua: *mut c_void) -> i32 {
    let lua = lua as usize;
    unsafe {
        let config = match flir_config_args(lua) {
            Ok(config) => config,
            Err(reason) => return flir_config_failed(lua, reason),
        };
        let Some(component) = mc_component(lua) else {
            return flir_config_failed(lua, NO_MC_STATE);
        };
        let Some((camera, source_node)) = mc_camera(component) else {
            log_mc_failure(lua);
            return flir_config_failed(lua, NO_CAMERA);
        };

        {
            let mut configs = FLIR_CONFIG
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if configs.len() >= FLIR_CONFIG_MAX && !configs.contains_key(&camera) {
                configs.clear();
            }
            let entry = configs.entry(camera).or_default();
            *entry = FlirCfg {
                enabled: entry.enabled,
                enabled_ms: entry.enabled_ms,
                ..config
            };
        }

        // One-shot in-game validation of the inferred link walk: the first
        // successful call proves which camera the MC is actually feeding.
        static LOGGED: AtomicBool = AtomicBool::new(false);
        if !LOGGED.swap(true, Ordering::Relaxed) {
            log_line(&format!(
                "ir.flir_config: camera={camera:#x} mc={component:#x} source_node={source_node:#x} \
slot_vt={:#x} camera_vt={:#x} cfg=({:?}, black_hot={}, window=[{}, {}] C)",
                read_usize_at(camera + CAMERA_VIDEO_SLOT),
                read_usize_at(camera),
                config.rgb,
                config.black_hot,
                config.tmin,
                config.tmax
            ));
        }
        lua_push_boolean(lua, true);
    }
    1
}

/// Parse and validate the six `ir.flir_config` arguments.
unsafe fn flir_config_args(lua: usize) -> Result<FlirCfg, &'static str> {
    let mut rgb = [0.0f32; 3];
    for (index, slot) in rgb.iter_mut().enumerate() {
        let Some(value) = lua_arg_number(lua, index as i32 + 1) else {
            return Err("r, g and b must be numbers (0..1 tint)");
        };
        if !value.is_finite() {
            return Err("r, g and b must be finite numbers");
        }
        *slot = value.clamp(0.0, 1.0) as f32;
    }
    let Some(black_hot) = lua_arg_bool(lua, 4) else {
        return Err("black_hot must be a boolean or a number");
    };
    let (Some(tmin), Some(tmax)) = (lua_arg_number(lua, 5), lua_arg_number(lua, 6)) else {
        return Err("tmin and tmax must be numbers (Celsius)");
    };
    if !tmin.is_finite() || !tmax.is_finite() {
        return Err("tmin and tmax must be finite numbers");
    }
    // Convert first: an f64 beyond the f32 range becomes infinite, which the
    // shader would then receive as a window edge.
    let (tmin, tmax) = (tmin as f32, tmax as f32);
    if !tmin.is_finite() || !tmax.is_finite() || !(tmin < tmax) {
        return Err("tmin must be below tmax");
    }
    if tmax - tmin < FLIR_MIN_WINDOW_C {
        return Err("the display window must be at least 0.01 C wide");
    }
    Ok(FlirCfg {
        rgb,
        black_hot,
        tmin,
        tmax,
        ..FlirCfg::default()
    })
}

/// Push `false` for a failed config call and log the reason once, so a script
/// calling this every tick cannot flood the log before the camera is wired.
fn flir_config_failed(lua: usize, reason: &str) -> i32 {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        log_line(&format!(
            "ir.flir_config returned false: {reason} (only the first failure is logged)"
        ));
    }
    unsafe { lua_push_boolean(lua, false) };
    1
}

/// `ir.flir(on)` - microcontroller only. Enables FLIR for the camera feeding
/// this MC's video input; must be called every tick (expires after 1 s).
/// `false`/`0` disables immediately. Returns true if a camera was resolved.
pub(crate) extern "C" fn ir_flir(lua: *mut c_void) -> i32 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { flir_inner(lua) }))
        .unwrap_or(0)
}

unsafe fn flir_inner(lua: *mut c_void) -> i32 {
    let lua = lua as usize;
    unsafe {
        let resolved = (|| {
            let on = lua_arg_bool(lua, 1)?;
            let component = mc_component(lua)?;
            let Some((camera, _)) = mc_camera(component) else {
                log_mc_failure(lua);
                return None;
            };
            Some((on, camera))
        })();
        let Some((on, camera)) = resolved else {
            lua_push_boolean(lua, false);
            return 1;
        };
        {
            let mut configs = FLIR_CONFIG
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if configs.len() >= FLIR_CONFIG_MAX && !configs.contains_key(&camera) {
                configs.clear();
            }
            let entry = configs.entry(camera).or_default();
            entry.enabled = on;
            entry.enabled_ms = crate::state::now_ms();
        }
        lua_push_boolean(lua, true);
    }
    1
}

// One heat source as seen by the IR sensor.
pub(crate) extern "C" fn ir_scan(lua: *mut c_void) -> i32 {
    // A panic must never unwind through the Lua C function: the game aborts.
    #[cfg(feature = "logging")]
    let t0 = std::time::Instant::now();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { scan_inner(lua) }));
    #[cfg(feature = "logging")]
    crate::blocktemp::SCAN_COST.add(t0);
    r
        .unwrap_or(0)
}

unsafe fn scan_inner(lua: *mut c_void) -> i32 {
    // Synchronous on the game thread: reuse readable() results within the scan
    // (a no-op when the scene-tick scope is already active around the script).
    let _regions = crate::memory::RegionCacheScope::new();
    let lua = lua as usize;
    unsafe {
        // Azimuth steering is clamped to the same 0.25-turn deflection the
        // elevation axis has by construction, so a steering command cannot
        // swing the scan cone past the gimbal limit.
        let az_steer = lua_arg_number(lua, 1)
            .unwrap_or(0.0)
            .clamp(-STEER_LIMIT_TURNS, STEER_LIMIT_TURNS);
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
        // arg 12: whether to return the cell table. A seeker whose image reaches
        // the microcontroller through ir.frame (rather than a composite) can skip
        // it: at 64x64 that is 4096 Lua table insertions per scan for values
        // nobody reads. An empty table is still returned, to keep the order of
        // the other return values.
        let cells_out = lua_arg_number(lua, 12).unwrap_or(1.0) != 0.0;


        let cell_count = (res_x * res_y) as usize;
        let mut cells = vec![0.0f64; cell_count];
        let mut az_error = 0.0f64;
        let mut el_error = 0.0f64;
        let mut strength = 0.0f64;
        let mut total_flux = 0.0f64;
        let mut base_temp = 20.0f64;
        let mut source_count = 0usize;

        // Diagnostics-only scan state for the throttled verbose log.
        #[cfg(feature = "logging")]
        let mut diag_fwd = (0.0f64, 0.0f64, 0.0f64);
        #[cfg(feature = "logging")]
        let mut diag_visible: Vec<(f64, f64, f64, f64, f64)> = Vec::new();
        #[cfg(feature = "logging")]
        let mut diag_fov = 0.0f64;
        #[cfg(feature = "logging")]
        let mut diag_cone = 0.0f64;
        #[cfg(feature = "logging")]
        let mut diag_steer = (0.0f64, 0.0f64);
        // Normalised centroid (wx/ws, wy/ws, weight_sum) for the throttled
        // dump - lets a pinned-at-centre error be told apart from a real one.
        #[allow(unused_assignments)]
        let mut diag_centroid = (0.0f64, 0.0f64, 0.0f64);
        let mut packed_vals: Vec<f64> = Vec::new();

        let component = lua_component(lua);
        let seeker_id = seeker_id(component);
        #[allow(unused_assignments)]
        let mut origin = (0.0f64, 0.0f64, 0.0f64);
        // The scan runs whenever the component exists; without a temperature
        // evaluation yet (no engine/probe this session) heat sources and the
        // real ambient are unavailable, but fires/flares still are (fires
        // holder cached by the fire-render hook).
        // A foreign Lua state can hand back a pointer that is not a component at
        // all, so the vtable is checked before anything is read through it.
        if component != 0 && crate::hooks::looks_like_component(component) {
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

            // One line per plugin load: proves which cone geometry is running
            // and records the configured FOV, so "is the game executing the
            // new build" is answerable from the player build's bootstrap log.
            static CONE_LOGGED: AtomicBool = AtomicBool::new(false);
            if !CONE_LOGGED.swap(true, Ordering::Relaxed) {
                log_line(&format!(
                    "scan armed: fov={fov:.4} turns ({:.2} deg full, {:.2} deg half-angle), \nerror sources: in-cone centroid only",
                    fov_rad.to_degrees(),
                    (fov_rad * 0.5).to_degrees()
                ));
            }

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
            // shortcut: the air at the seeker stands in for the air at each
            // source and for the scene background; per-source air if it matters.
            base_temp = base_ambient_at(origin) as f64;
            for i in 0..vcount {
                let a = variant_args(i);
                collect_ir_sources(a[0], &origin, range, base_temp, &mut sources);
                collect_volume_heat_sources(a[0], &origin, range, base_temp, &mut sources);
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
            collect_emitter_sources(&origin, range, &mut sources);
            // A seeker sees contrast: only radiance above the background's, so
            // something at or below the air temperature is invisible.
            let l_bg = crate::ir::band_radiance_c(base_temp as f32);
            for s in sources.iter_mut() {
                let l = crate::ir::band_radiance_c(s.temp_c as f32);
                s.flux *= if l > 0.0 { (1.0 - l_bg / l).max(0.0) } else { 0.0 };
            }
            let own_vehicle = crate::blocktemp::seeker_vehicle(component);
            // The game's heat records carry no vehicle; drop the ones inside
            // the seeker's own vehicle so it can't lock onto its own engine.
            if let Some(vid) = own_vehicle {
                let spheres = crate::blocktemp::vehicle_spheres(vid);
                sources.retain(|s| {
                    !spheres.iter().any(|(c, r)| {
                        (s.x - c.0).powi(2) + (s.y - c.1).powi(2) + (s.z - c.2).powi(2) < (r + 0.5).powi(2)
                    })
                });
            }
            add_sun_source(origin, &mut sources);
            // Water is opaque to IR: a submerged seeker sees nothing, and
            // nothing submerged is seen.
            let sea = crate::blocktemp::SEA_LEVEL_IR_M;
            if origin.1 < sea {
                sources.clear();
            }
            sources.retain(|s| s.sky || s.y >= sea);
            source_count = sources.len();
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
                        // Out-of-cone sources are dropped below anyway; a
                        // physics ray each was most of the scan's cost.
                        let d = s.az - az_center;
                        let daz = (d - TAU * (d / TAU).round()) * s.el.cos().max(0.2);
                        if (daz * daz + (s.el - el_center).powi(2)).sqrt() > fov_rad * 0.5 {
                            return true;
                        }
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

            // Pre-cone snapshot for verbose diagnostics only.
            #[cfg(feature = "logging")]
            {
                diag_fwd = forward;
                diag_fov = fov_rad;
                diag_cone = fov_rad * 0.5;
                diag_steer = (az_center, el_center);
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
            }

            // Guidance comes from source directions, not the raster centroid:
            // a sub-pixel emitter can straddle cells symmetrically and make
            // that centroid exactly boresight while the source is off-axis.
            // Only sources inside the configured FOV may steer the seeker. A
            // target outside it leaves the errors at zero, so a missile whose
            // seeker is pointed away flies straight instead of being pulled
            // onto whatever heat source happens to be in front of the hull.
            // The sun steers too: in the cone it outshines any target.
            // Same rule as the original plugin: the single brightest source.
            let mut brightest_in_cone: Option<(f64, f64, f64)> = None;
            for s in &sources {
                if !s.flux.is_finite() || s.flux <= 0.0 {
                    continue;
                }
                let d = s.az - az_center;
                let daz = d - TAU * (d / TAU).round();
                let del = s.el - el_center;
                let off = ((daz * s.el.cos().max(0.2)).powi(2) + del * del).sqrt();
                if off <= fov_rad * 0.5
                    && brightest_in_cone.map_or(true, |best| s.flux > best.0)
                {
                    brightest_in_cone = Some((s.flux, daz, del));
                }
            }
            if let Some((_, daz, del)) = brightest_in_cone {
                // Absolute angular offsets in turns, independent of FOV and
                // pixel resolution (0.37 degrees is about 0.00103 turns).
                az_error = daz / TAU;
                el_error = del / TAU;
            }
            // Evidence for oscillation: any scan-to-scan error step over
            // 0.0005 turns (0.18 deg), with every in-cone source behind it.
            #[cfg(feature = "logging")]
            {
                static LAST_ERR: std::sync::Mutex<Option<std::collections::HashMap<u32, (f64, f64)>>> =
                    std::sync::Mutex::new(None);
                static LAST_STEP_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let mut g = LAST_ERR.lock().unwrap_or_else(|e| e.into_inner());
                let m = g.get_or_insert_with(std::collections::HashMap::new);
                if let Some(&(pa, pe)) = m.get(&seeker_id) {
                    let step = (az_error - pa).abs().max((el_error - pe).abs());
                    let now = crate::state::now_ms();
                    if step > 0.0005 && now.saturating_sub(LAST_STEP_LOG.load(Ordering::Relaxed)) > 200 {
                        LAST_STEP_LOG.store(now, Ordering::Relaxed);
                        let list: Vec<String> = sources
                            .iter()
                            .filter(|s| {
                                let d = s.az - az_center;
                                let daz = d - TAU * (d / TAU).round();
                                ((daz * s.el.cos().max(0.2)).powi(2) + (s.el - el_center).powi(2)).sqrt() <= fov_rad * 0.5
                            })
                            .map(|s| {
                                let d = ((s.x - origin.0).powi(2) + (s.y - origin.1).powi(2) + (s.z - origin.2).powi(2)).sqrt();
                                format!("[flux={:.4} T={:.0}C d={:.0}m r={:.2} az={:+.3} el={:+.3}{}]", s.flux, s.temp_c, d, s.r_eff,
                                    s.az.to_degrees(), s.el.to_degrees(), if s.sky { " SKY" } else { "" })
                            })
                            .collect();
                        log_line(&format!(
                            "seeker {seeker_id}: error step ({:+.5},{:+.5}) -> ({:+.5},{:+.5}) turns steer=({:+.2},{:+.2}) deg own_vehicle={:?} in-cone: {}",
                            pa, pe, az_error, el_error, az_center.to_degrees(), el_center.to_degrees(), own_vehicle, list.join(" ")
                        ));
                    }
                }
                if m.len() < 256 || m.contains_key(&seeker_id) {
                    m.insert(seeker_id, (az_error, el_error));
                }
            }

            // The configured FOV still gates the rendered image, detection
            // flux and source count; steering above is independent of raster.
            sources.retain(|s| {
                let d = s.az - az_center;
                let daz = d - TAU * (d / TAU).round();
                let del = s.el - el_center;
                let a = daz * s.el.cos().max(0.2);
                let inside = (a * a + del * del).sqrt() <= fov_rad * 0.5;
                if inside {
                    total_flux += s.flux;
                }
                inside
            });
            source_count = sources.len();

            // Snapshot the survivors for the heartbeat: distance, effective
            // radius and flux are enough to identify an emitter and back-solve
            // its temperature, which is exactly what a faint reading needs.
            if let Ok(mut snapshot) = LAST_SOURCES.lock() {
                snapshot.clear();
                for s in sources.iter().take(8) {
                    let dx = s.x - origin.0;
                    let dy = s.y - origin.1;
                    let dz = s.z - origin.2;
                    snapshot.push((
                        (dx * dx + dy * dy + dz * dz).sqrt(),
                        s.r_eff,
                        s.flux,
                        s.sky,
                        s.temp_c,
                    ));
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
                // How far past full scale this source's core would clip, which
                // is the light available to bloom into the surrounding cells.
                let excess = if gain > 1e-9 {
                    (s.flux * dilut / gain - 1.0).max(0.0)
                } else {
                    0.0
                };
                let glare = BLOOM_PEAK * (1.0 - (-excess).exp());
                // Only visit cells the source can reach: its disc (or the
                // 1.2-cell point spill) and, when it blooms, the halo out to
                // 3 radii, where it is below one 12-bit step.
                let (mut rc, mut rr) = if solid <= cell_solid {
                    (1.2, 1.2)
                } else {
                    (
                        s.theta / (az_sp * s.el.cos().max(0.2)),
                        s.theta / el_sp,
                    )
                };
                if glare > 0.0 {
                    rc = rc.max(3.0 * BLOOM_RADIUS_CELLS);
                    rr = rr.max(3.0 * BLOOM_RADIUS_CELLS);
                }
                let span = |c: f64, r: f64, n: i32| {
                    ((c - r).floor().max(0.0) as i32, ((c + r).ceil() as i32).min(n - 1))
                };
                let (c0, c1) = span(col_f, rc, res_x);
                let (r0, r1) = span(row_f, rr, res_y);
                for row in r0..=r1 {
                    for col in c0..=c1 {
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
                        let idx = (row * res_x + col) as usize;
                        let halo = glare
                            * (-(d_cells / BLOOM_RADIUS_CELLS).powi(2)).exp()
                            * gain;
                        let nv = (s.flux * dilut * w).max(halo);
                        if nv > cells[idx] {
                            cells[idx] = nv;
                        }
                    }
                }
            }

            // Keep the raw image centroid for diagnostics only. Guidance is
            // computed from source angles above, before FOV rasterization, so a
            // sub-pixel emitter cannot disappear into a symmetric pixel blob.
            for (index, value) in cells.iter().enumerate() {
                let col = (index as i32 % res_x) as f64;
                let row = (index as i32 / res_x) as f64;
                weight_sum += value;
                weight_x += value * (col + 0.5) / res_x as f64;
                weight_y += value * (0.5 - (row + 0.5) / res_y as f64);
            }
            diag_centroid = (
                weight_x / weight_sum.max(1e-9),
                weight_y / weight_sum.max(1e-9),
                weight_sum,
            );

            // Normalize the display image independently of guidance.
            for value in cells.iter_mut() {
                let norm = if gain > 1e-9 { *value / gain } else { 0.0 };
                *value = norm.clamp(0.0, 1.0);
            }
            strength = if gain > 1e-9 {
                (total_flux / gain).clamp(0.0, 1.0)
            } else {
                0.0
            };

            // Publish the rendered frame for the drawing environment, under this
            // seeker's identity. A logic composite is 32 floats - 64 packed
            // cells - so a 64x64 image can only leave the component through the
            // Lua state; ir.frame(id) reads this snapshot from the
            // microcontroller side.
            {
                let bytes: Vec<u8> = cells
                    .iter()
                    .map(|value| (value.clamp(0.0, 1.0) * 255.0) as u8)
                    .collect();
                let mut frames = LAST_FRAME
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if frames.len() >= FRAME_MAP_MAX && !frames.contains_key(&seeker_id) {
                    frames.clear();
                }
                frames.insert(seeker_id, (res_x, res_y, bytes));
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
        // Per-scan error trace while something is tracked: PN guidance
        // differentiates the line of sight every tick, so tick-to-tick
        // jitter is what matters and the 1-in-300 dump can't show it.
        #[cfg(feature = "logging")]
        if strength > 0.0 {
            static TRACE_LINES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if TRACE_LINES.fetch_add(1, Ordering::Relaxed) < 6000 {
                log_line(&format!(
                    "trace seeker {seeker_id}: err=({:+.6},{:+.6}) turns flux={:.5} n={}",
                    az_error, el_error, total_flux, source_count
                ));
            }
        }
        #[cfg(feature = "logging")]
        if scan_index % 300 == 0 {
            {
                let ready = TEMP_READY.load(Ordering::SeqCst);
                log_line(&format!(
                    "scan: sources={} total_flux={:.5} base={} strength {:.3} \
error=({:+.6},{:+.6}) turns centroid wx/ws={:.4} wy/ws={:.4} cells_lit={} peak={:.3}",
                    source_count,
                    total_flux,
                    if ready {
                        format!("{:.1} C", base_temp)
                    } else {
                        "n/a".to_string()
                    },
                    strength,
                    az_error,
                    el_error,
                    diag_centroid.0,
                    diag_centroid.1,
                    cells.iter().filter(|v| **v > 0.0).count(),
                    cells.iter().cloned().fold(0.0f64, f64::max),
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
                log_volume_heat_sources(a[0]);
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
        if cells_out {
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
        }
        lua_push_number(lua, az_error);
        lua_push_number(lua, el_error);
        lua_push_number(lua, strength);
        lua_push_number(lua, total_flux);
        lua_push_number(lua, base_temp);
        lua_push_number(lua, source_count as f64);
        lua_push_number(lua, seeker_id as f64);
    }
    8
}


// ---------------------------------------------------------------------------
// vector / matrix helpers
// ---------------------------------------------------------------------------
