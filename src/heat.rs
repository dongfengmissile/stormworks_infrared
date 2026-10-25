//! The game's heat model: the world root the temperature evaluators are called
//! with, the point temperature and the ambient base. Also the verbose container
//! dumps.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::ir::Vec3;
use crate::logging::log_line;
use crate::memory::{
    game_fn, in_game_image, read_u32_at, read_usize_at, readable, writable, PTR_MIN,
};
// Used by the verbose container dumps only.
#[cfg(feature = "logging")]
use crate::memory::{read_f32_at, read_f64_at, PTR_MAX};
use crate::offsets::*;
use crate::state::*;

/// Diagnostic: describe the cached heat-source container (ring list) and a few
/// records, so we can tell whether a nearby engine is even a heat-field source.
/// Diagnostic dump of one heat container variant (logging builds only).
#[cfg(feature = "logging")]
pub(crate) unsafe fn log_heat_container(c: usize) {
    if DEEP_DUMP_DISABLED.load(Ordering::Relaxed) {
        return;
    }
    // c can be a stale captured hook argument and every pointer below is
    // game-controlled, so EVERY dereference must be readable()-guarded. A
    // range check alone passes on any garbage pointer inside userland.
    if c < PTR_MIN || !readable(c, 0x34) {
        log_line("heat container: NULL or unreadable");
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
    if slots < PTR_MIN || cap == 0 || count == 0 {
        return;
    }
    let dump = count.min(32);
    for i in 0..dump {
        let idx = ((start as usize + i as usize) % cap as usize) as usize;
        if !readable(slots + idx * 8, 8) {
            continue;
        }
        let holder = read_usize_at(slots + idx * 8);
        if !(PTR_MIN..PTR_MAX).contains(&holder) || !readable(holder, 8) {
            continue;
        }
        let rec = read_usize_at(holder);
        if !(PTR_MIN..PTR_MAX).contains(&rec) || !readable(rec, 0x128) {
            continue;
        }
        let mode = *(rec as *const u8);
        let cx = read_f64_at(rec + 0x68);
        let cy = read_f64_at(rec + 0x70);
        let cz = read_f64_at(rec + 0x78);
        let radius = read_f32_at(rec + 0x108);
        let ext1 = read_f32_at(rec + 0x10c);
        let ext2 = read_f32_at(rec + 0x110);
        let factor = read_f32_at(rec + 0x114);
        let temp = read_f32_at(rec + 0x118);
        let owner = read_usize_at(rec + 0x120);
        if mode != 0 {
            // Box mode: print all three full extents (e0 is at +0x108) so the
            // component type can be fingerprinted from the box shape.
            log_line(&format!(
                "  heat[{idx}] mode={mode} c=({cx:.1},{cy:.1},{cz:.1}) ext=({radius:.2},{ext1:.2},{ext2:.2}) f={factor:.3} scale={temp:.1} owner={owner:#x}"
            ));
        } else {
            log_line(&format!(
                "  heat[{idx}] mode={mode} c=({cx:.1},{cy:.1},{cz:.1}) r={radius:.2} f={factor:.3} scale={temp:.1} owner={owner:#x}"
            ));
        }
    }
}

pub(crate) fn cache_variant(args: &[usize; 8]) {
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

pub(crate) fn variant_args(i: usize) -> [usize; 8] {
    let base = i * 8;
    let mut a = [0usize; 8];
    for j in 0..8 {
        a[j] = VARIANT_ARGS[base + j].load(Ordering::Relaxed);
    }
    a
}

// ---------------------------------------------------------------------------
// World root
// ---------------------------------------------------------------------------

// Offsets of the world fields the game's temperature evaluators receive as
// arguments. The root itself is recovered once (through one of them, or from
// the scene the hooks cache) and re-validated on every use, so a call-scoped
// hook argument can never become a cached call argument again.
const WORLD_HEAT_FIELD: usize = 0xbaef0;
const WORLD_WIND_FIELD: usize = 0x68a0;
const WORLD_WEATHER: usize = 0xbade8;
const WORLD_ENV_LIST: usize = 0x4ce0;
const WORLD_PROVIDER: usize = 0x4988;

/// Water flag the game's callers pass for a craft/interior position (1), which
/// is where a seeker camera or a vehicle body always sits.
const EVAL_WATER_FLAG: u8 = 1;

/// Returned while no validated world root exists (menu, level load/teardown).
pub(crate) const WORLD_FALLBACK_C: f32 = 20.0;

static WORLD_ROOT: AtomicUsize = AtomicUsize::new(0);
static WORLD_LOGGED: AtomicBool = AtomicBool::new(false);
static WORLD_FAIL_LOGGED: AtomicBool = AtomicBool::new(false);

/// Validate a world root: the weather grid header, the heat field and the wind
/// field are the structures both evaluators dereference through it. A torn-down
/// world must fail this instead of being called.
unsafe fn world_ok(world: usize) -> bool {
    // Userland, 8-aligned, and low enough that every offset below cannot wrap.
    if !(PTR_MIN..0x8000_0000_0000).contains(&world) || world & 7 != 0 {
        return false;
    }
    if !readable(world + WORLD_WEATHER, 0xb8)
        || !readable(world + WORLD_HEAT_FIELD, 8)
        || !readable(world + WORLD_WIND_FIELD, 8)
    {
        return false;
    }
    // Weather grid: uint16 cell count at +6 (air/sea cell tables at +0/+0x28 and
    // the height range at +0xb0/+0xb4, docs/RE_NOTES.md §3.1). The neighbouring
    // pointer fields are cell DATA addresses inside the grid, not userland
    // pointers: they are only ever dereferenced by the evaluator itself, so
    // validating them here would reject every real world.
    if ((read_u32_at(world + WORLD_WEATHER + 4) >> 16) as u16) < 2 {
        return false;
    }
    read_usize_at(world + WORLD_HEAT_FIELD) != 0
}

/// The validated world root, or `None` while the game has none. Cached once,
/// but re-derived whenever the cached value stops validating so a level reload
/// does not leave every probe on the fallback.
///
/// The cached scene pointer is preferred: the scene tick and the render uniform
/// fill both hand it over every frame, so it is the freshest copy of the same
/// object the evaluator's arguments point at.
/// Drop the cached world root (the scene object changed: world load/unload).
pub(crate) fn forget_world() {
    WORLD_ROOT.store(0, Ordering::Relaxed);
}

pub(crate) unsafe fn world() -> Option<usize> {
    let cached = WORLD_ROOT.load(Ordering::Relaxed);
    let live = SCENE.load(Ordering::Relaxed);
    if cached != 0 {
        // A freed scene from the previous world can stay readable and pass
        // world_ok; the scene tick's current object is the authority.
        if (live == 0 || live == cached) && world_ok(cached) {
            return Some(cached);
        }
        WORLD_ROOT.store(0, Ordering::Relaxed);
    }
    let scene = SCENE.load(Ordering::Relaxed);
    let found = if scene != 0 && world_ok(scene) {
        Some((scene, "cached scene"))
    } else {
        // Recover the root from one of the evaluator's own arguments (cached by
        // the temperature hook, never used as call arguments any more).
        let args = variant_args(0);
        let mut hit = None;
        for (index, field, name) in [
            (0usize, WORLD_HEAT_FIELD, "temperature arg2 - 0xbaef0"),
            (1, WORLD_WIND_FIELD, "temperature arg3 - 0x68a0"),
            (2, WORLD_WEATHER, "temperature arg4 - 0xbade8"),
            (3, WORLD_ENV_LIST, "temperature arg5 - 0x4ce0"),
            (5, WORLD_PROVIDER, "temperature arg7 - 0x4988"),
        ] {
            if args[index] >= field + PTR_MIN && world_ok(args[index] - field) {
                hit = Some((args[index] - field, name));
                break;
            }
        }
        hit
    };
    let Some((world, source)) = found else {
        // A failed validation must never be silent: without a root every probe
        // is the fixed fallback, which otherwise looks like a broken sim.
        if !WORLD_FAIL_LOGGED.swap(true, Ordering::Relaxed) {
            log_line(&format!(
                "world root not found (scene={scene:#x}); temperature probes report the {WORLD_FALLBACK_C} C fallback"
            ));
        }
        return None;
    };
    WORLD_ROOT.store(world, Ordering::Relaxed);
    if !WORLD_LOGGED.swap(true, Ordering::Relaxed) {
        log_line(&format!(
            "world root {world:#x} from {source} (scene={scene:#x} same={})",
            world == scene
        ));
    }
    Some(world)
}

/// Out parameter of [`fake_provider_get`]: the evaluator hands the provider its
/// output pointer in RDX. The call site has no guard at all, so nothing here may
/// fault: return 0 C as well, and write only into memory proven writable.
unsafe extern "win64" fn fake_provider_get(_this: usize, out: usize, _unused: usize) -> f32 {
    if writable(out, 4) {
        *(out as *mut f32) = 0.0;
    }
    0.0
}

// A stand-in for the vehicle air-volume provider: the evaluator reads the
// provided temperature through vtable slot +0x28 (index 5) only, so a slot that
// reports 0 C ("no room air volume here") reproduces the caller's behaviour.
// Atomic cells rather than statics with pointer initialisers: a static cannot.
static FAKE_PROVIDER_VT: [AtomicUsize; 6] = [const { AtomicUsize::new(0) }; 6];
static FAKE_PROVIDER: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];

fn fake_provider() -> usize {
    if FAKE_PROVIDER[0].load(Ordering::Relaxed) == 0 {
        FAKE_PROVIDER_VT[5].store(
            fake_provider_get as unsafe extern "win64" fn(usize, usize, usize) -> f32 as usize,
            Ordering::Relaxed,
        );
        FAKE_PROVIDER[0].store(FAKE_PROVIDER_VT.as_ptr() as usize, Ordering::Relaxed);
    }
    FAKE_PROVIDER.as_ptr() as usize
}

/// The game's own provider object, but only when both its vtable and the slot
/// the evaluator calls are game image memory; otherwise the static stand-in.
unsafe fn provider_object(world: usize) -> usize {
    let provider = world + WORLD_PROVIDER;
    if readable(provider, 8) {
        let vtable = read_usize_at(provider);
        if in_game_image(vtable) && readable(vtable + 0x28, 8) {
            if in_game_image(read_usize_at(vtable + 0x28)) {
                return provider;
            }
        }
    }
    fake_provider()
}

// ---------------------------------------------------------------------------
// hooks
// ---------------------------------------------------------------------------

/// Base ambient temperature at a world point (no heat sources added).
///
/// `ambient_temp_at_pos(pos, windField, weatherGrid, ignored, waterFlag)`: a4 is
/// dead in the callee (only its home slot is used as scratch), so it gets a
/// local buffer instead of a cached object.
pub(crate) unsafe fn base_ambient_at(origin: Vec3) -> f32 {
    let Some(world) = world() else {
        return WORLD_FALLBACK_C;
    };
    base_ambient_in(world, origin)
}

/// [`base_ambient_at`] for a caller that already holds a validated world root.
unsafe fn base_ambient_in(world: usize, origin: Vec3) -> f32 {
    let pos = [origin.0, origin.1, origin.2, 0.0f64];
    let mut scratch = 0u64;
    let ambient_fn: unsafe extern "C" fn(usize, usize, usize, usize, u8) -> f32 =
        game_fn(RVA_BASE_TEMP);
    let value = ambient_fn(
        pos.as_ptr() as usize,
        world + WORLD_WIND_FIELD,
        world + WORLD_WEATHER,
        &mut scratch as *mut u64 as usize,
        EVAL_WATER_FLAG,
    );
    if value.is_finite() {
        value
    } else {
        WORLD_FALLBACK_C
    }
}

pub(crate) unsafe fn temperature_at(x: f64, y: f64, z: f64) -> f32 {
    crate::fault::set(crate::fault::stage::HEAT_TEMPERATURE_AT);
    let Some(world) = world() else {
        return WORLD_FALLBACK_C;
    };
    let pos = [x, y, z, 0.0f64];
    let pptr = pos.as_ptr() as usize;

    // The game's own evaluator (fires, lava, hot objects, the vehicle air-volume
    // provider), with every argument rebuilt from the validated world root:
    //   temperature_eval_at_pos(pos, heat, wind, weather, envList, unused,
    //                           provider, waterFlag, airVolume)
    let original = ORIG_TEMPERATURE.load(Ordering::SeqCst);
    let eval = if original != 0 {
        original
    } else {
        game_fn::<usize>(RVA_TEMPERATURE)
    };
    if eval == 0 {
        return WORLD_FALLBACK_C;
    }
    let f: unsafe extern "C" fn(usize, usize, usize, usize, usize, usize, usize, u8, usize) -> f32 =
        std::mem::transmute(eval);
    let value = f(
        pptr,
        world + WORLD_HEAT_FIELD,
        world + WORLD_WIND_FIELD,
        world + WORLD_WEATHER,
        world + WORLD_ENV_LIST,
        0,
        provider_object(world),
        EVAL_WATER_FLAG,
        0,
    );
    if value.is_finite() {
        value
    } else {
        WORLD_FALLBACK_C
    }
}

/// Diagnostic dump of the fires container (logging builds only).
#[cfg(feature = "logging")]
pub(crate) unsafe fn log_fires_container(c: usize) {
    if DEEP_DUMP_DISABLED.load(Ordering::Relaxed) {
        return;
    }
    if c < PTR_MIN || !readable(c, 0x11ec) {
        log_line("fires container: NULL or unreadable");
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
    if slots < PTR_MIN || cap == 0 || count == 0 {
        return;
    }
    let dump = (count as usize).min(4);
    for i in 0..dump {
        let idx = ((start as usize + i) % cap as usize) as usize;
        if !readable(slots + idx * 8, 8) {
            continue;
        }
        let group = read_usize_at(slots + idx * 8);
        if !(PTR_MIN..PTR_MAX).contains(&group) || !readable(group, 0x98) {
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

/// Diagnostic dump of the heat container's second ring (volume heat emitters).
#[cfg(feature = "logging")]
pub(crate) unsafe fn log_volume_heat_sources(c: usize) {
    if DEEP_DUMP_DISABLED.load(Ordering::Relaxed) {
        return;
    }
    if c < PTR_MIN || !readable(c, 0x1c) {
        return;
    }
    let slots = read_usize_at(c + 0x8);
    let cap = read_u32_at(c + 0x10);
    let start = read_u32_at(c + 0x14);
    let count = read_u32_at(c + 0x18);
    log_line(&format!(
        "volume-heat container {:#x}: slots={:#x} cap={} start={} count={}",
        c, slots, cap, start, count
    ));
    if slots < PTR_MIN || cap == 0 || count == 0 {
        return;
    }
    let dump = (count as usize).min(6);
    for i in 0..dump {
        let idx = ((start as usize + i) % cap as usize) as usize;
        if !readable(slots + idx * 8, 8) {
            continue;
        }
        let holder = read_usize_at(slots + idx * 8);
        if !(PTR_MIN..PTR_MAX).contains(&holder) || !readable(holder, 8) {
            continue;
        }
        let rec = read_usize_at(holder);
        if !(PTR_MIN..PTR_MAX).contains(&rec) || !readable(rec, 0x58) {
            continue;
        }
        log_line(&format!(
            "  vheat[{}] c=({:.1},{:.1},{:.1}) heat={:.1} C radius={:.2}",
            idx,
            read_f64_at(rec + 0x40),
            read_f64_at(rec + 0x48),
            read_f64_at(rec + 0x50),
            read_f32_at(rec),
            read_f32_at(rec + 4)
        ));
    }
}
