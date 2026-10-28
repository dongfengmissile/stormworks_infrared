//! Process-wide mutable state shared between the hooks, the collectors and
//! the Lua API. Kept separate from `offsets` so the layout of the game is
//! never mixed with our own runtime bookkeeping.

const std = @import("std");
const win = @import("win32.zig");
const fx = @import("fx.zig");

pub const gpa = std.heap.c_allocator;

const Atomic = std.atomic.Value;

pub var SELF_MODULE: Atomic(usize) = .init(0);
/// The directory holding this DLL (marker files live there). Set once by the
/// bootstrap thread before any hook is installed.
pub var PLUGIN_DIR: ?win.WidePath = null;
pub var MODULE_BASE: Atomic(usize) = .init(0);
pub var ORIG_REGISTRAR: Atomic(usize) = .init(0);
pub var ORIG_TEMPERATURE: Atomic(usize) = .init(0);
pub var ORIG_FIRES_BUILD: Atomic(usize) = .init(0);
pub var FIRES_HOLDER: Atomic(usize) = .init(0);
/// Largest fire intensity seen this session (the float at group+0x94), as f32
/// bits, reported in the heartbeat. The game scales a 5 m linear falloff by
/// this value; the plugin's radiometry treats it as a multiplier on a fixed
/// 1 m 1000 C emitter, so its real range decides whether that model is wrong
/// by a constant factor or is simply reading a small per-fire value.
pub var FIRE_INTENSITY_MAX_BITS: Atomic(u32) = .init(0);
/// Largest intensity among entries whose +0x91 flag marks a real fire
/// (param_5 == 0 in FUN_140300110, scaled by 400) as opposed to smoke
/// (non-zero, scaled by 50). The plugin currently models every ring entry as a
/// fire, so these two maxima show whether the ring it reads is fires or smoke.
pub var FIRE_ONLY_MAX_BITS: Atomic(u32) = .init(0);
pub var SMOKE_ONLY_MAX_BITS: Atomic(u32) = .init(0);
pub var ORIG_SCENE_TICK: Atomic(usize) = .init(0);
pub var SCENE: Atomic(usize) = .init(0);
pub var HOOKS_INSTALLED: Atomic(bool) = .init(false);
pub var REGISTRAR_HITS: Atomic(usize) = .init(0);
pub var ORIG_SCREEN_INIT: Atomic(usize) = .init(0);
pub var SCREEN_INIT_HITS: Atomic(usize) = .init(0);

/// One seeker's published image: one 0..255 value per cell, row-major.
pub const Frame = struct {
    width: i32,
    height: i32,
    bytes: std.ArrayList(u8) = .empty,
};
/// Each seeker's last rendered image, published for the drawing
/// (microcontroller) Lua environment, keyed by the seeker id its script scans
/// with. A vehicle can carry several seekers, so this is a map rather than one
/// slot - otherwise the last seeker to tick would overwrite the others.
pub var LAST_FRAME: win.Mutex(std.AutoHashMapUnmanaged(u32, Frame)) = .{ .value = .empty };

// Exhaust-emitter capture: the creation hook records each emitter's effect
// index, the per-tick update hook appends {x,y,z,rate,temp} records and ir.scan
// drains them. One record per emitter, so emitters stay throttle-scaled and
// despawn-safe with no ring lifetime bookkeeping.
pub var EXHAUST_HOOKS_DISABLED: Atomic(bool) = .init(false);
pub var ROCKET_HOOKS_DISABLED: Atomic(bool) = .init(false);
pub var JET_HOOKS_DISABLED: Atomic(bool) = .init(false);
pub var PRESET_HOOKS_DISABLED: Atomic(bool) = .init(false);
pub var MODULAR_HOOKS_DISABLED: Atomic(bool) = .init(false);
pub var FLUID_HOOKS_DISABLED: Atomic(bool) = .init(false);
pub var SCREEN_HOOKS_DISABLED: Atomic(bool) = .init(false);
/// Kill switch for the FLIR render side (new GL-level hooks): a prologue or
/// runtime mismatch already disables it, this lets a user do so up front.
pub var FLIR_DISABLED: Atomic(bool) = .init(false);
/// FLIR block substitution off, composite still on (`infrared_disable_blockpass`).
pub var BLOCKPASS_DISABLED: Atomic(bool) = .init(false);
/// Block thermal simulation off: `blocktemp.tick` is never called
/// (`infrared_disable_sim`).
pub var SIM_DISABLED: Atomic(bool) = .init(false);
/// FLIR composite off, block substitution still on
/// (`infrared_disable_composite`).
pub var COMPOSITE_DISABLED: Atomic(bool) = .init(false);
pub var ORIG_EXHAUST_UPDATE: Atomic(usize) = .init(0);
pub var ORIG_ROCKET_TICK: Atomic(usize) = .init(0);
pub var ORIG_JET_TICK: Atomic(usize) = .init(0);
pub var ORIG_PRESET_TICK: Atomic(usize) = .init(0);
pub var ORIG_MODULAR_TICK: Atomic(usize) = .init(0);
pub var ORIG_WATER_TICK: Atomic(usize) = .init(0);
pub var ORIG_STEAM_TICK: Atomic(usize) = .init(0);
// Effect indices already reported, so the diagnostics print each one once.
pub var SEEN_INDICES: win.Mutex(std.ArrayList(u32)) = .{ .value = .empty };

/// One tracked engine-family component, refreshed by the tick hooks and dumped
/// (then cleared) by the periodic diagnostics. This is what makes individual
/// engines identifiable in the log: family, peak state over the interval and the
/// latest world position.
pub const EngineSample = struct {
    key: usize,
    family: []const u8,
    max_power: f64,
    max_heat: f64,
    x: f64,
    y: f64,
    z: f64,
    /// How many times this component has ticked (drives per-component probes).
    calls: u32,
    /// Physical size (m) of the block, from its definition's voxel bounds.
    size_m: f64,
    /// Latest fuel/progress counter (+0x468). Its per-tick drop is a burn
    /// signal that works even where the throttle-derived power stays quiet.
    progress: f32,
    /// Latest value of +0x7a4: the solid propellant level on fuel segments
    /// (1.0 == full) and the burn-rate setting on the booster itself. Its drop
    /// is what says a solid booster is actually burning.
    propellant: f32,
    drain: f32,
    /// Latest burn-enable byte (+0x1d0). When it is clear the tick skips its
    /// burn block entirely, so +0x778 keeps its previous value and must not be
    /// trusted as "still burning".
    burning: bool,
};
// Rolling engine-block telemetry, sampled by the periodic dump and reset there so
// each line covers one interval. Non-negative f64 bit patterns are ordered, so
// fetchMax works directly on the bits.
pub var ENGINE_SAMPLES: win.Mutex(std.ArrayList(EngineSample)) = .{ .value = .empty };
pub var JET_TICK_CALLS: Atomic(usize) = .init(0);
pub var PRESET_TICK_CALLS: Atomic(usize) = .init(0);
pub var MODULAR_TICK_CALLS: Atomic(usize) = .init(0);
pub var WATER_TICK_CALLS: Atomic(usize) = .init(0);
pub var STEAM_TICK_CALLS: Atomic(usize) = .init(0);
pub var ROCKET_TICK_CALLS: Atomic(usize) = .init(0);
pub var JET_MAX_POWER_BITS: Atomic(u64) = .init(0);
pub var JET_MAX_HEAT_BITS: Atomic(u64) = .init(0);
pub var ROCKET_MAX_POWER_BITS: Atomic(u64) = .init(0);

/// A pending engine/plume source.
pub const Emitter = struct {
    key: usize,
    /// Last-seen stamp (ms).
    seen_ms: u64,
    x: f64,
    y: f64,
    z: f64,
    activity: f64,
    temp_c: f64,
    size_m: f64,
};
/// Pending engine/plume sources. A despawned component never ticks again, so
/// its entry would radiate forever as a phantom; readers drop anything older
/// than EMITTER_STALE_MS rather than trusting the hooks to retract it.
pub var EMITTER_PENDING: win.Mutex(std.ArrayList(Emitter)) = .{ .value = .empty };

/// Keys in EMITTER_PENDING that are exhaust plume emitters (not components):
/// blocktemp heats the blocks around these as heat points instead of binding
/// them to a component's cells.
pub var EXHAUST_EMITTERS: win.Mutex(std.ArrayList(usize)) = .{ .value = .empty };

/// Entries older than this are dropped as phantoms of despawned components.
/// Anything alive refreshes its entry every tick, so 2 s is generous.
pub const EMITTER_STALE_MS: u64 = 2000;

/// Kill switch for the verbose build's deep field dumps (ring walkers and
/// class-field readers). They are readable()-guarded, but if the verbose
/// build ever still crashes the game, they stay off unless opted in:
/// INFRARED_DEEPDUMP=1 or an `infrared_enable_deepdump` marker file next to
/// the DLL.
pub var DEEP_DUMP_DISABLED: Atomic(bool) = .init(true);

/// Milliseconds since the plugin loaded - the freshness stamp for state that
/// only a live component's tick can refresh.
pub fn nowMs() u64 {
    return win.elapsedMs();
}

/// Microseconds since the plugin loaded (short-interval polling thresholds).
pub fn nowUs() u64 {
    return win.elapsedUs();
}

/// One in-cone source of the most recent scan.
pub const SourceSnapshot = struct {
    dist_m: f64,
    r_eff: f64,
    flux: f64,
    sky: bool,
    temp_c: f64,
};
/// In-cone sources from the most recent scan, bounded so the heartbeat line
/// stays small. Exists so a faint reading can be attributed to a specific
/// emitter from the player build - the verbose build is unusable while it has
/// an unexplained crash.
pub var LAST_SOURCES: win.Mutex(std.ArrayList(SourceSnapshot)) = .{ .value = .empty };

/// One jet part INSTANCE.
pub const JetPart = struct {
    component: usize,
    definition: usize,
    /// Part type from definition+0xed4.
    part_type: u32,
    /// Afterburner lit.
    afterburner: u32,
    /// RPS at +0x1bc.
    rps: f64,
    /// Field at +0x1d8.
    heat: f64,
    /// Field at +0x1cc.
    exhaust: f64,
    /// Field at +0x1d0.
    d0: f64,
    size_m: f64,
    /// Published IR temperature (C).
    ir_temp_c: f64,
    seen_ms: u64,
};
/// One entry per jet part INSTANCE, keyed by component pointer so two copies
/// of the same part definition (a twin engine's two exhausts) each get a line.
/// A jet multiblock ticks once per part, so this also exposes the turbine count
/// that a jet's signature is supposed to scale with. Bounded, and read by the
/// player-build heartbeat because the verbose build is unusable while it
/// crashes.
pub var JET_PARTS: win.Mutex(std.ArrayList(JetPart)) = .{ .value = .empty };

// Arguments seen by FUN_14030fd60. p2 is the (global) heat-source container and
// p9 is the owner filter the game compares under equality to record+0x120, so
// replaying them only ever exposes one owner's heat sources. We therefore cache
// the container/args per distinct container and evaluate every record directly.
// Stored lock-free because the scan samples in a tight loop.
pub const MAX_VARIANTS: usize = 4;
pub var TEMP_READY: Atomic(bool) = .init(false);
pub var VARIANT_ARGS: [8 * MAX_VARIANTS]Atomic(usize) = [_]Atomic(usize){.init(0)} ** (8 * MAX_VARIANTS);
pub var VARIANT_COUNT: Atomic(usize) = .init(0);

pub var TEMP_FIRST: Atomic(bool) = .init(false);
pub var SCAN_COUNT: Atomic(usize) = .init(0);

// ---------------------------------------------------------------------------
// FLIR camera view (docs/FLIR_DESIGN.md)
// ---------------------------------------------------------------------------

/// Per-camera FLIR presentation. Missing entry = defaults (white hot, RGB 1,1,1,
/// window [-5, 60] C). Keyed by the camera component pointer.
pub const FlirCfg = struct {
    rgb: [3]f32 = .{ 1.0, 1.0, 1.0 },
    black_hot: bool = false,
    tmin: f32 = -5.0,
    tmax: f32 = 60.0,
    /// Set by `ir.flir`; only honoured while fresh (see `flirEnabledCfg`).
    enabled: bool = false,
    enabled_ms: u64 = 0,
};

/// `ir.flir` must be re-called within this window or FLIR switches off.
pub const FLIR_ENABLE_TTL_MS: u64 = 1000;

/// The camera's FLIR config, only if `ir.flir(true)` was called within 1 s.
pub fn flirEnabledCfg(camera: usize) ?FlirCfg {
    const map = FLIR_CONFIG.lock();
    defer FLIR_CONFIG.unlock();
    const cfg = map.get(camera) orelse return null;
    if (cfg.enabled and nowMs() -| cfg.enabled_ms <= FLIR_ENABLE_TTL_MS) return cfg;
    return null;
}

pub var FLIR_CONFIG: win.Mutex(fx.HashMap(usize, FlirCfg)) = .{ .value = .empty };
/// Ambient air temperature (°C) at the scene, as f32 bits, refreshed on the game
/// thread by `flir.sceneTick` (median of the thermal sim's body samples). NaN
/// bits mean "not sampled yet": the composite then uses its fixed fallback. It
/// is never computed from the render thread (the heat evaluator is game-thread
/// only).
pub var FLIR_SCENE_AMBIENT_BITS: Atomic(u32) = .init(0x7fc0_0000);

// FLIR hook trampolines (installed by src/flir.zig).
pub var ORIG_TONEMAP: Atomic(usize) = .init(0);
pub var ORIG_GL_DRAW_ELEMENTS: Atomic(usize) = .init(0);
pub var ORIG_GL_UNIFORM_MATRIX4FV: Atomic(usize) = .init(0);
pub var ORIG_GL_BIND_FRAMEBUFFER: Atomic(usize) = .init(0);

// ---------------------------------------------------------------------------
// logging
// ---------------------------------------------------------------------------

/// Diagnostics captured for the throttled sun dump.
pub var SUN_SCENE_LOG: Atomic(usize) = .init(0);
pub var SUN_CLOCK_LOG: Atomic(usize) = .init(0);

pub var ORIG_SUN_UPDATE: Atomic(usize) = .init(0);
pub var ORIG_FRAME_UNIFORMS: Atomic(usize) = .init(0);
/// Sun light direction captured from the frame state the renderer uploads as
/// sun_light_direction (uniform fill arg 11 + 0x11e doubles).
pub var LIGHT_DIR: win.Mutex([3]f64) = .{ .value = .{ 0, 0, 0 } };
pub var LIGHT_DIR_VALID: Atomic(bool) = .init(false);
pub var SUN_LOG: Atomic(usize) = .init(0);
pub var SUN_DIR: win.Mutex([3]f64) = .{ .value = .{ 0, 0, 0 } };
pub var SUN_DIR_VALID: Atomic(bool) = .init(false);
pub var SUN_PCT: Atomic(usize) = .init(0); // day percent x1000 from the sun hook

pub var SUN_EL: Atomic(isize) = .init(0); // deg x10, for the log
pub var SUN_AZ: Atomic(isize) = .init(0); // deg x10, for the log
