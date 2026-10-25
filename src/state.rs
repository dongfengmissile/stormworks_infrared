//! Process-wide mutable state shared between the hooks, the collectors and
//! the Lua API. Kept separate from `offsets` so the layout of the game is
//! never mixed with our own runtime bookkeeping.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, AtomicUsize};
use std::sync::Mutex;


pub(crate) static SELF_MODULE: AtomicUsize = AtomicUsize::new(0);
pub(crate) static MODULE_BASE: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ORIG_REGISTRAR: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ORIG_TEMPERATURE: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ORIG_FIRES_BUILD: AtomicUsize = AtomicUsize::new(0);
pub(crate) static FIRES_HOLDER: AtomicUsize = AtomicUsize::new(0);
/// Largest fire intensity seen this session (the float at group+0x94), as f32
/// bits, reported in the heartbeat. The game scales a 5 m linear falloff by
/// this value; the plugin's radiometry treats it as a multiplier on a fixed
/// 1 m 1000 C emitter, so its real range decides whether that model is wrong
/// by a constant factor or is simply reading a small per-fire value.
pub(crate) static FIRE_INTENSITY_MAX_BITS: AtomicU32 = AtomicU32::new(0);
/// Largest intensity among entries whose +0x91 flag marks a real fire
/// (param_5 == 0 in FUN_140300110, scaled by 400) as opposed to smoke
/// (non-zero, scaled by 50). The plugin currently models every ring entry as a
/// fire, so these two maxima show whether the ring it reads is fires or smoke.
pub(crate) static FIRE_ONLY_MAX_BITS: AtomicU32 = AtomicU32::new(0);
pub(crate) static SMOKE_ONLY_MAX_BITS: AtomicU32 = AtomicU32::new(0);
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
/// Kill switch for the FLIR render side (new GL-level hooks): a prologue or
/// runtime mismatch already disables it, this lets a user do so up front.
pub(crate) static FLIR_DISABLED: AtomicBool = AtomicBool::new(false);
/// FLIR block substitution off, composite still on (`infrared_disable_blockpass`).
pub(crate) static BLOCKPASS_DISABLED: AtomicBool = AtomicBool::new(false);
/// Block thermal simulation off: `blocktemp::tick` is never called
/// (`infrared_disable_sim`).
pub(crate) static SIM_DISABLED: AtomicBool = AtomicBool::new(false);
/// FLIR composite off, block substitution still on
/// (`infrared_disable_composite`).
pub(crate) static COMPOSITE_DISABLED: AtomicBool = AtomicBool::new(false);
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
/// Pending engine/plume sources: (key, last-seen ms, x, y, z, activity,
/// temperature, size m). A despawned component never ticks again, so its entry
/// would radiate forever as a phantom; readers drop anything older than
/// EMITTER_STALE_MS rather than trusting the hooks to retract it.
pub(crate) static EMITTER_PENDING: Mutex<Vec<(usize, u64, f64, f64, f64, f64, f64, f64)>> =
    Mutex::new(Vec::new());

/// Keys in EMITTER_PENDING that are exhaust plume emitters (not components):
/// blocktemp heats the blocks around these as heat points instead of binding
/// them to a component's cells.
pub(crate) static EXHAUST_EMITTERS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Entries older than this are dropped as phantoms of despawned components.
/// Anything alive refreshes its entry every tick, so 2 s is generous.
pub(crate) const EMITTER_STALE_MS: u64 = 2000;

/// Kill switch for the verbose build's deep field dumps (ring walkers and
/// class-field readers). They are readable()-guarded, but if the verbose
/// build ever still crashes the game, this turns them all off without a
/// rebuild: INFRARED_NO_DEEPDUMP=1 or an `infrared_disable_deepdump` marker
/// file next to the log.
pub(crate) static DEEP_DUMP_DISABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Milliseconds since the plugin loaded - the freshness stamp for state that
/// only a live component's tick can refresh.
pub(crate) fn now_ms() -> u64 {
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// Microseconds since the plugin loaded (short-interval polling thresholds).
pub(crate) fn now_us() -> u64 {
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_micros() as u64
}

/// In-cone sources from the most recent scan as (distance m, effective radius
/// m, flux, sky), bounded so the heartbeat line stays small. Exists so a faint
/// reading can be attributed to a specific emitter from the player build - the
/// verbose build is unusable while it has an unexplained crash.
pub(crate) static LAST_SOURCES: Mutex<Vec<(f64, f64, f64, bool, f64)>> = Mutex::new(Vec::new());

/// One entry per jet part INSTANCE, keyed by component pointer so two copies
/// of the same part definition (a twin engine's two exhausts) each get a line.
/// (component, definition pointer, part type from definition+0xed4, afterburner
/// lit, RPS at +0x1bc, field at +0x1d8, field at +0x1cc, field at +0x1d0,
/// block size m, published IR temp C, last-seen ms).
/// A jet multiblock ticks once per part, so this also exposes the turbine count
/// that a jet's signature is supposed to scale with. Bounded, and read by the
/// player-build heartbeat because the verbose build is unusable while it
/// crashes.
pub(crate) static JET_PARTS:
    Mutex<Vec<(usize, usize, u32, u32, f64, f64, f64, f64, f64, f64, u64)>> = Mutex::new(Vec::new());

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
// FLIR camera view (docs/FLIR_DESIGN.md)
// ---------------------------------------------------------------------------

/// Per-camera FLIR presentation. Missing entry = defaults (white hot, RGB 1,1,1,
/// window [-5, 60] C). Keyed by the camera component pointer.
#[derive(Clone, Copy)]
pub(crate) struct FlirCfg {
    pub rgb: [f32; 3],
    pub black_hot: bool,
    pub tmin: f32,
    pub tmax: f32,
    /// Set by `ir.flir`; only honoured while fresh (see `flir_enabled_cfg`).
    pub enabled: bool,
    pub enabled_ms: u64,
}

impl Default for FlirCfg {
    fn default() -> Self {
        FlirCfg {
            rgb: [1.0, 1.0, 1.0],
            black_hot: false,
            tmin: -5.0,
            tmax: 60.0,
            enabled: false,
            enabled_ms: 0,
        }
    }
}

/// `ir.flir` must be re-called within this window or FLIR switches off.
pub(crate) const FLIR_ENABLE_TTL_MS: u64 = 1000;

/// The camera's FLIR config, only if `ir.flir(true)` was called within 1 s.
pub(crate) fn flir_enabled_cfg(camera: usize) -> Option<FlirCfg> {
    let map = FLIR_CONFIG.lock().unwrap_or_else(|p| p.into_inner());
    let cfg = *map.get(&camera)?;
    (cfg.enabled && now_ms().saturating_sub(cfg.enabled_ms) <= FLIR_ENABLE_TTL_MS).then_some(cfg)
}

pub(crate) static FLIR_CONFIG: std::sync::LazyLock<Mutex<HashMap<usize, FlirCfg>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
/// Ambient air temperature (°C) at the scene, as f32 bits, refreshed on the game
/// thread by `flir::scene_tick` (median of the thermal sim's body samples). NaN
/// bits mean "not sampled yet": the composite then uses its fixed fallback. It
/// is never computed from the render thread (the heat evaluator is game-thread
/// only).
pub(crate) static FLIR_SCENE_AMBIENT_BITS: AtomicU32 = AtomicU32::new(0x7fc0_0000);

// FLIR hook trampolines (installed by src/flir.rs).
pub(crate) static ORIG_TONEMAP: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ORIG_GL_DRAW_ELEMENTS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ORIG_GL_UNIFORM_MATRIX4FV: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ORIG_GL_BIND_FRAMEBUFFER: AtomicUsize = AtomicUsize::new(0);

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
