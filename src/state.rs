//! Process-wide mutable state shared between the hooks, the collectors and
//! the Lua API. Kept separate from `offsets` so the layout of the game is
//! never mixed with our own runtime bookkeeping.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, AtomicUsize};
use std::sync::Mutex;


pub(crate) static SELF_MODULE: AtomicUsize = AtomicUsize::new(0);
pub(crate) static MODULE_BASE: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ORIG_REGISTRAR: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ORIG_TEMPERATURE: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ORIG_FIRES_BUILD: AtomicUsize = AtomicUsize::new(0);
pub(crate) static FIRES_HOLDER: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ORIG_SCENE_TICK: AtomicUsize = AtomicUsize::new(0);
pub(crate) static SCENE: AtomicUsize = AtomicUsize::new(0);
pub(crate) static HOOKS_INSTALLED: AtomicBool = AtomicBool::new(false);
pub(crate) static REGISTRAR_HITS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ORIG_SCREEN_INIT: AtomicUsize = AtomicUsize::new(0);
pub(crate) static SCREEN_INIT_HITS: AtomicUsize = AtomicUsize::new(0);
/// Each seeker's last rendered image, published for the drawing
/// (microcontroller) Lua environment, keyed by the channel its script scans on:
/// (width, height, one 0..255 value per cell, row-major). A vehicle can carry
/// several seekers, so this is a map rather than one slot - otherwise the last
/// seeker to tick would overwrite the others.
pub(crate) static LAST_FRAME: Mutex<BTreeMap<u32, (i32, i32, Vec<u8>)>> =
    Mutex::new(BTreeMap::new());
pub(crate) static LOG_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

// Exhaust-emitter capture: the creation hook records each emitter's effect
// index, the per-tick update hook appends {x,y,z,rate,temp} records and ir.scan
// drains them. One record per emitter, so emitters stay throttle-scaled and
// despawn-safe with no ring lifetime bookkeeping.
pub(crate) static EXHAUST_HOOKS_DISABLED: AtomicBool = AtomicBool::new(false);
pub(crate) static ROCKET_HOOKS_DISABLED: AtomicBool = AtomicBool::new(false);
pub(crate) static JET_HOOKS_DISABLED: AtomicBool = AtomicBool::new(false);
pub(crate) static PRESET_HOOKS_DISABLED: AtomicBool = AtomicBool::new(false);
pub(crate) static MODULAR_HOOKS_DISABLED: AtomicBool = AtomicBool::new(false);
pub(crate) static FLUID_HOOKS_DISABLED: AtomicBool = AtomicBool::new(false);
pub(crate) static SCREEN_HOOKS_DISABLED: AtomicBool = AtomicBool::new(false);
pub(crate) static ORIG_EXHAUST_UPDATE: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ORIG_ROCKET_TICK: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ORIG_JET_TICK: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ORIG_PRESET_TICK: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ORIG_MODULAR_TICK: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ORIG_WATER_TICK: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ORIG_STEAM_TICK: AtomicUsize = AtomicUsize::new(0);
// Effect indices already reported, so the diagnostics print each one once.
pub(crate) static SEEN_INDICES: Mutex<Vec<u32>> = Mutex::new(Vec::new());
// Rolling engine-block telemetry, sampled by the periodic dump and reset there so
// each line covers one interval. Non-negative f64 bit patterns are ordered, so
// fetch_max works directly on the bits.
/// One tracked engine-family component, refreshed by the tick hooks and dumped
/// (then cleared) by the periodic diagnostics. This is what makes individual
/// engines identifiable in the log: family, peak state over the interval and the
/// latest world position.
pub(crate) struct EngineSample {
    pub key: usize,
    pub family: &'static str,
    pub max_power: f64,
    pub max_heat: f64,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    /// How many times this component has ticked (drives per-component probes).
    pub calls: u32,
    /// Physical size (m) of the block, from its definition's voxel bounds.
    pub size_m: f64,
    /// Latest fuel/progress counter (+0x468). Its per-tick drop is a burn
    /// signal that works even where the throttle-derived power stays quiet.
    pub progress: f32,
    /// Latest value of +0x7a4: the solid propellant level on fuel segments
    /// (1.0 == full) and the burn-rate setting on the booster itself. Its drop
    /// is what says a solid booster is actually burning.
    pub propellant: f32,
    pub drain: f32,
    /// Latest burn-enable byte (+0x1d0). When it is clear the tick skips its
    /// burn block entirely, so +0x778 keeps its previous value and must not be
    /// trusted as "still burning".
    pub burning: bool,
}
pub(crate) static ENGINE_SAMPLES: Mutex<Vec<EngineSample>> = Mutex::new(Vec::new());
pub(crate) static JET_TICK_CALLS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static PRESET_TICK_CALLS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static MODULAR_TICK_CALLS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static WATER_TICK_CALLS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static STEAM_TICK_CALLS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ROCKET_TICK_CALLS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static JET_MAX_POWER_BITS: AtomicU64 = AtomicU64::new(0);
pub(crate) static JET_MAX_HEAT_BITS: AtomicU64 = AtomicU64::new(0);
pub(crate) static ROCKET_MAX_POWER_BITS: AtomicU64 = AtomicU64::new(0);
// (emitter, x, y, z, volume, temperature) - one live record per emitter, so a
// per-tick update just refreshes its entry instead of growing the list.
/// Pending engine/plume sources: (key, x, y, z, activity, temperature, size m).
pub(crate) static EMITTER_PENDING: Mutex<Vec<(usize, f64, f64, f64, f64, f64, f64)>> =
    Mutex::new(Vec::new());

// Arguments seen by FUN_14030fd60. p2 is the (global) heat-source container and
// p9 is the owner filter the game compares under equality to record+0x120, so
// replaying them only ever exposes one owner's heat sources. We therefore cache
// the container/args per distinct container and evaluate every record directly.
// Stored lock-free because the scan samples in a tight loop.
pub(crate) const MAX_VARIANTS: usize = 4;
pub(crate) static TEMP_READY: AtomicBool = AtomicBool::new(false);
pub(crate) static VARIANT_ARGS: [AtomicUsize; 8 * MAX_VARIANTS] =
    [const { AtomicUsize::new(0) }; 8 * MAX_VARIANTS];
pub(crate) static VARIANT_COUNT: AtomicUsize = AtomicUsize::new(0);

pub(crate) static TEMP_FIRST: AtomicBool = AtomicBool::new(false);
pub(crate) static SCAN_COUNT: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// logging
// ---------------------------------------------------------------------------

/// Diagnostics captured for the throttled sun dump.
pub(crate) static SUN_SCENE_LOG: AtomicUsize = AtomicUsize::new(0);
pub(crate) static SUN_CLOCK_LOG: AtomicUsize = AtomicUsize::new(0);

pub(crate) static ORIG_SUN_UPDATE: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ORIG_FRAME_UNIFORMS: AtomicUsize = AtomicUsize::new(0);
/// Sun light direction captured from the frame state the renderer uploads as
/// sun_light_direction (uniform fill arg 11 + 0x11e doubles).
pub(crate) static LIGHT_DIR: Mutex<[f64; 3]> = Mutex::new([0.0; 3]);
pub(crate) static LIGHT_DIR_VALID: AtomicBool = AtomicBool::new(false);
pub(crate) static SUN_LOG: AtomicUsize = AtomicUsize::new(0);
pub(crate) static SUN_DIR: Mutex<[f64; 3]> = Mutex::new([0.0; 3]);
pub(crate) static SUN_DIR_VALID: AtomicBool = AtomicBool::new(false);
pub(crate) static SUN_PCT: AtomicUsize = AtomicUsize::new(0); // day percent x1000 from the sun hook

pub(crate) static SUN_EL: AtomicIsize = AtomicIsize::new(0); // deg x10, for the log
pub(crate) static SUN_AZ: AtomicIsize = AtomicIsize::new(0); // deg x10, for the log
