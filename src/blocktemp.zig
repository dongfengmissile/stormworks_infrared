//! Per-block thermal simulation for the FLIR view.
//!
//! Every voxel cell of every vehicle block carries a temperature (C). The game
//! thread advances the sim from the scene-tick detour; the render thread reads
//! the published per-body grids through `publishedLock`.
//!
//! Model (explicit Euler, one lumped temperature per 0.25 m voxel cell):
//!
//! ```text
//! C dT/dt = h_env A (T_env - T) + k sum_n (T_n - T) + solar - h_air A (T - T_air)
//! ```
//!
//! * `T_env` is `T_air` except near a burning voxel: the game's per-body
//!   `compartment_fires` octrees are read directly (`FireField`), because the
//!   game's own heat field only models a fire as one 5 m blob at a random burning
//!   cell. Within 0.4 m of a burning voxel it is `FIRE_TEMP_C`, falling off
//!   linearly to `T_air` at 1.0 m.
//! * `T_air` is the game's ambient base (`heat.baseAmbientAt`).
//! * `C = mass_per_cell * c_specific`; a component's `def+0x2b0` mass is split
//!   over its voxel cells, so heavy blocks heat and cool slowly.
//! * Conduction runs to the six face neighbours inside the same body and is
//!   symmetric, so it conserves energy.
//! * Solar gain uses the day-gated direction already cached by `sun`/`hooks`
//!   and a constant flux (no per-face normal: one absorbed face per cell).
//!
//! Bodies are interlaced: each one is re-enumerated and stepped only every
//! `SIM_INTERLACE` ticks with `dt * SIM_INTERLACE`. A body pays for one
//! ambient probe per update. Fires are re-read every `FIRE_SCAN_INTERVAL_MS`
//! with a bounded node budget (`MAX_FIRE_NODES_PER_SCAN`).
//! Temperatures persist across ticks and save/load because cells are keyed by
//! the game's stable `(vehicle_id, body_id, voxel)` tuple.
//!
//! All game pointers come from the scene walk and are range/`readable`-checked
//! before use; counts, set sizes and grid dimensions are capped, and anything
//! that fails validation is skipped rather than trusted.

const std = @import("std");
const fault = @import("fault.zig");
const flir = @import("flir.zig");
const fx = @import("fx.zig");
const heat = @import("heat.zig");
const logging = @import("logging.zig");
const memory = @import("memory.zig");
const offsets = @import("offsets.zig");
const state = @import("state.zig");
const sun = @import("sun.zig");
const util = @import("util.zig");
const win = @import("win32.zig");

const Atomic = std.atomic.Value;
const gpa = state.gpa;
const verbose = logging.verbose;
const logOnce = logging.logOnce;
const logf = logging.logf;
const readable = memory.readable;
const readUsizeAt = memory.readUsizeAt;
const readU32At = memory.readU32At;
const readF32At = memory.readF32At;
const readF64At = memory.readF64At;
const PTR_MIN = memory.PTR_MIN;
const PTR_MAX = memory.PTR_MAX;
const stage = fault.stage;

pub const Vec3 = [3]f64;

fn readI32At(addr: usize) i32 {
    return @bitCast(readU32At(addr));
}

// ---------------------------------------------------------------------------
// Offsets (facts about stormworks64.exe, see docs/RE_NOTES.md §2)
// ---------------------------------------------------------------------------

/// Vehicle record fields of the scene's record set (see `offsets.zig`).
const OFF_REC_VEHICLE_ID: usize = 0x04;
const OFF_REC_VEHICLE: usize = 0x08;
const OFF_REC_TYPE: usize = 0x10;
const REC_TYPE_VEHICLE: u32 = 5;
/// Vehicle -> set of bodies (set at +0x00).
const OFF_VEHICLE_BODIES: usize = 0x00;
/// Body -> set of components, and the body's stable id.
const OFF_BODY_COMPONENTS: usize = 0x3F0;
// body+0x2C8 is only the body->vehicle "local_transform"; the world transform lives in
// the body's physics object: *(body+0x718)+0x18 = f64 4x4 row-major (p·M), applied to
// (voxel*0.25 - origin) with origin = f32x3 at body+0x720 (as the component ticks do).
const OFF_BODY_PHYSICS: usize = 0x718;
const OFF_PHYSICS_WORLD: usize = 0x18;
const OFF_BODY_ORIGIN: usize = 0x720;
const OFF_BODY_ID: usize = 0x147C;
/// Component: voxel position, voxel-space rotation `int[9]`, definition.
const OFF_COMP_VOXEL: usize = 0x18;
const OFF_COMP_ROT: usize = 0x30;
const OFF_COMP_DEF: usize = 0x58;
/// Definition: per-component mass, voxel-cell set (60-byte entries, int3 at 0).
const OFF_DEF_MASS: usize = 0x2B0;
const OFF_DEF_CELLS: usize = 0xA8;
const CELL_ENTRY_BYTES: usize = 60;
/// Body -> `compartment_fires`: ptr to 104-byte records / record count.
const OFF_BODY_FIRES: usize = 0x1218;
const OFF_BODY_FIRE_COUNT: usize = 0x1220;
const FIRE_RECORD_BYTES: usize = 104;
/// Body +0x1228: inline default record (receives every burning cell no
/// fire-source record claims); `body+0x1220` counts only the source records.
const OFF_BODY_DEFAULT_FIRE: usize = 0x1228;
/// Record: cell count (i32) at +0, 8 octree root pointers at +0x08, root count at +0x48.
const OFF_FIRE_CELLS: usize = 0x00;
const OFF_FIRE_ROOTS: usize = 0x08;
/// Octree node: 8 child slots (+0..0x38) + u32 subtree count (+0x40); 0x48 bytes.
/// At size 2 each slot points at an 8-byte box whose qword is the leaf pointer.
const NODE_BYTES: usize = 0x48;
/// Leaf (0x28 B): slot vector ptr +0, cap +8, start +0xC, count +0x10, int3 cell +0x18.
const OFF_LEAF_SLOTS: usize = 0x00;
const OFF_LEAF_CAP: usize = 0x08;
const OFF_LEAF_START: usize = 0x0C;
const OFF_LEAF_COUNT: usize = 0x10;
const OFF_NODE_POS: usize = 0x18;
/// Slot item stride and active-flag byte offset (ring of {type, timer, leaf, active}).
const LEAF_SLOT_BYTES: usize = 0x18;
const OFF_SLOT_ACTIVE: usize = 0x10;
/// The octree covers body-local voxels `[-FIRE_HALF, FIRE_HALF)^3`.
const FIRE_HALF: i32 = 512;

// ---------------------------------------------------------------------------
// Tunables (one line each: change these to taste, nothing else depends on them)
// ---------------------------------------------------------------------------

/// Voxel cell edge, metres (the game's grid scale).
const CELL_M: f32 = 0.25;
/// One face of a voxel cell, m^2.
const FACE_AREA_M2: f32 = CELL_M * CELL_M;
/// Lumped specific heat, J/(kg K); ~concrete/metal-ish, one number for all blocks.
const SPECIFIC_HEAT_J_KG_K: f32 = 500.0;
/// Coupling to the game's point temperature (fire / heat field), W/(m^2 K) per
/// exposed face; flame radiation + convection is ~50-100, so a burning cell heats in ~1 min.
// 200 W/m2K * 0.0625 m2 = 12.5 W/K, the same as one conduction link: a part
// touching a hot neighbour body (landing gear on a wing) heats like a block.
const H_ENV_W_M2_K: f32 = 200.0;
/// Coupling to ambient air, W/(m^2 K).
const H_AIR_W_M2_K: f32 = 4.0;
/// Voxel-to-voxel conduction, W/K per face.
// Steel-like: k≈50 W/(m·K) × 0.0625 m² / 0.25 m ≈ 12.5 W/K per shared face.
const K_COND_W_PER_K: f32 = 12.5;
/// Absorbed solar flux on one cell face in full daylight, W/m^2.
const SOLAR_FLUX_W_M2: f32 = 400.0;
/// Largest fraction of a cell's heat content one step may move (0..1 stability).
const MAX_STEP_FRACTION: f32 = 0.5;
/// Bodies per interlace cycle; each is stepped with `dt * SIM_INTERLACE`.
const SIM_INTERLACE: u32 = 4;
/// Game cap for fire heat, C (`min(400, ..)` in the heat evaluator).
const FIRE_TEMP_C: f32 = 400.0;
/// Full fire temperature within this distance of a burning voxel, m.
const FIRE_FULL_M: f64 = 0.4;
/// `T_env` falls linearly to `T_air` at this distance, m.
const FIRE_FADE_M: f64 = 1.0;
/// Spatial hash bucket edge, m (>= `FIRE_FADE_M`, so 3x3x3 buckets suffice).
const FIRE_BUCKET_M: f64 = 1.0;
/// Fire octrees are re-read at most this often.
const FIRE_SCAN_INTERVAL_MS: u64 = 250;
/// Octree nodes visited per scan across all bodies.
const MAX_FIRE_NODES_PER_SCAN: u32 = 20_000;
/// Fire records read per body.
const MAX_FIRE_RECORDS: u32 = 64;
/// Burning voxels kept per scan.
const MAX_FIRE_VOXELS: usize = 20_000;
/// Heat points (fire + hot source cells) kept per scan.
const MAX_HEAT_POINTS: usize = 20_000;
/// Source cells hotter than ambient by more than this radiate to other cells, C.
const SOURCE_RADIATE_MARGIN_C: f32 = 5.0;
/// A burning voxel may lie this far outside the body's known cell box, voxels.
const FIRE_BOX_MARGIN: i32 = 16;
/// Per-body fire log/dump interval (verbose builds).
const FIRE_LOG_INTERVAL_MS: u64 = 5000;
/// A source only binds when its published world position is within this many
/// metres of the component's voxel (covers multi-voxel parts and one tick of
/// vehicle motion); otherwise the entry belongs to a different frame.
const SOURCE_MAX_DIST_M: f64 = 4.0;
/// Hottest-cell diagnostics per body (verbose builds).
const HOT_LOG_INTERVAL_MS: u64 = 5000;
/// Mass assumed for a cell whose definition has no usable mass.
const FALLBACK_MASS_KG_PER_CELL: f32 = 1.0;
/// Lower bound on heat capacity so a near-massless cell cannot divide by ~0.
const MIN_CAPACITY_J_K: f32 = 1.0;
/// Temperatures are clamped here; the game's own sources cap around 400 C.
const MIN_TEMP_C: f32 = -80.0;
const MAX_TEMP_C: f32 = 1200.0;
/// Time constant (s) with which a cell is pulled to its component's real
/// temperature; the coupling is `capacity / tau`, so heavy cells follow as fast.
const SOURCE_TAU_S: f32 = 1.5;
/// Engine source entries older than this are ignored.
const SOURCE_FRESH_MS: u64 = 1000;
/// Fallback when the game's ambient evaluator is not ready.
const DEFAULT_AIR_C: f32 = 20.0;
/// A step smaller than this does not bump a grid version (avoids needless uploads).
const TEMP_CHANGE_EPS_C: f32 = 0.005;
/// Scene tick delta bounds, seconds: paused/first tick uses the default.
const DEFAULT_TICK_DT_S: f32 = 1.0 / 60.0;
const MAX_TICK_DT_S: f32 = 0.25;
/// Persistent cells of bodies not enumerated for this long are dropped.
const STALE_BODY_MS: u64 = 3000;
/// How often stale cells/bodies are pruned.
const PRUNE_INTERVAL_MS: u64 = 5000;

// ---------------------------------------------------------------------------
// Safety caps (garbage pointers/counts must never drive a large loop/alloc)
// ---------------------------------------------------------------------------

const MAX_VEHICLES: u32 = 512;
const MAX_BODIES_PER_VEHICLE: u32 = 256;
/// Bodies the scene walk may advance per tick. Bodies beyond this are simply
/// left for a later tick (their published grid is dropped, not kept stale).
const MAX_BODIES_TOTAL: usize = 256;
const MAX_COMPONENTS_PER_BODY: u32 = 65_536;
const MAX_CELLS_PER_COMPONENT: u32 = 16_384;
const MAX_CELLS_PER_BODY: usize = 1 << 18;
/// Grid memory one map (staging) may hold, bytes. The published map the render
/// thread reads is a second copy, so the whole budget is ~2x this.
const MAX_GRID_BYTES: usize = 32 << 20;
/// Largest grid side, texels. Also the bound the render side checks the texture
/// dimensions against (`flir.bodyTexture`).
pub const MAX_GRID_DIM: u32 = 256;
/// Largest accepted set modulus (slot count) of a game set header.
pub const MAX_SET_MOD: u32 = 1 << 16;
const MAX_VOXEL_COORD: u32 = 1 << 20;

const NEIGHBOURS = [6][3]i32{
    .{ 1, 0, 0 },
    .{ -1, 0, 0 },
    .{ 0, 1, 0 },
    .{ 0, -1, 0 },
    .{ 0, 0, 1 },
    .{ 0, 0, -1 },
};

// ---------------------------------------------------------------------------
// Public interface (see docs/FLIR_DESIGN.md)
// ---------------------------------------------------------------------------

/// One body's temperature volume, in voxel space (`x` fastest, NaN = empty).
pub const BodyGrid = struct {
    /// Voxel coordinate of texel (0,0,0).
    min: [3]i32 = .{ 0, 0, 0 },
    /// Texels per axis.
    dims: [3]u32 = .{ 0, 0, 0 },
    /// Temperatures in Celsius, `len = dims[0]*dims[1]*dims[2]`.
    temps_c: std.ArrayList(f32) = .empty,
    /// Bumped whenever `temps_c` changes (the render thread's upload trigger).
    version: u64 = 0,

    fn deinit(self: *BodyGrid) void {
        self.temps_c.deinit(gpa);
    }

    /// Copy `other` into `self`, reusing `self`'s allocation.
    fn copyFrom(self: *BodyGrid, other: *const BodyGrid) bool {
        self.temps_c.clearRetainingCapacity();
        self.temps_c.appendSlice(gpa, other.temps_c.items) catch return false;
        self.min = other.min;
        self.dims = other.dims;
        self.version = other.version;
        return true;
    }
};

/// Advance the block thermal sim by one scene tick. Safe to call every tick;
/// only bodies whose interlace turn it is are re-enumerated, and the fire
/// octrees are re-read at most every `FIRE_SCAN_INTERVAL_MS`.
pub fn tick(scene: usize) void {
    const t0 = if (verbose) win.elapsedNs() else 0;
    tickInner(scene);
    if (verbose) TICK_COST.add(t0);
}

/// Game-thread CPU cost (sum, max, count in us) for the status line.
pub const Cost = struct {
    sum: Atomic(u64) = .init(0),
    max: Atomic(u64) = .init(0),
    n: Atomic(u64) = .init(0),

    /// Account the time since `t0_ns` (from `win.elapsedNs`).
    pub fn add(self: *Cost, t0_ns: u64) void {
        const us = (win.elapsedNs() -| t0_ns) / 1000;
        _ = self.sum.fetchAdd(us, .monotonic);
        _ = self.max.fetchMax(us, .monotonic);
        _ = self.n.fetchAdd(1, .monotonic);
    }

    const Taken = struct {
        sum: u64,
        max: u64,
        n: u64,
        pub fn format(self: Taken, writer: *std.Io.Writer) std.Io.Writer.Error!void {
            try writer.print("n={d} avg={d}us max={d}us total={d}ms", .{ self.n, self.sum / @max(self.n, 1), self.max, self.sum / 1000 });
        }
    };

    fn take(self: *Cost) Taken {
        return .{ .sum = self.sum.swap(0, .monotonic), .max = self.max.swap(0, .monotonic), .n = self.n.swap(0, .monotonic) };
    }
};
var TICK_COST: Cost = .{};
pub var SCAN_COST: Cost = .{};

/// Vehicles/bodies one scene walk saw (reported by the rate-limited status
/// line, and by anything else that wants the counts).
pub const WalkStats = struct {
    vehicles: u32 = 0,
    bodies: u32 = 0,
};

/// Lock the grids of the current frame (key = body pointer) for reading. The
/// lock is held until `publishedUnlock`, which keeps the upload path
/// allocation-free.
pub fn publishedLock() *const fx.HashMap(usize, BodyGrid) {
    return PUBLISHED.lock();
}

pub fn publishedUnlock() void {
    PUBLISHED.unlock();
}

/// Median ambient air temperature (°C) over the bodies the sim stepped recently
/// (game thread). Bodies colder or warmer than the air around the player cancel
/// out, which makes this a usable scene ambient for a view with no camera of its
/// own (the FLIR main view). Null when no body has been stepped yet.
pub fn medianBodyAmbient() ?f32 {
    return medianBody("air_c");
}

/// Median sea-level air temperature under the live bodies.
pub fn medianGroundAmbient() ?f32 {
    return medianBody("ground_air_c");
}

fn medianBody(comptime field: []const u8) ?f32 {
    const now = state.nowMs();
    const sim = SIM.lock();
    defer SIM.unlock();
    var values: std.ArrayList(f32) = .empty;
    defer values.deinit(gpa);
    var it = sim.bodies.valueIterator();
    while (it.next()) |meta| {
        if (now -| meta.last_seen_ms > 2_000) continue;
        const value = @field(meta, field);
        if (std.math.isFinite(value)) values.append(gpa, value) catch return null;
    }
    if (values.items.len == 0) return null;
    std.mem.sort(f32, values.items, {}, std.sort.asc(f32));
    return values.items[values.items.len / 2];
}

// ---------------------------------------------------------------------------
// Persistent state
// ---------------------------------------------------------------------------

/// Stable body identity; survives body-pointer churn and save/load.
const BodyKey = struct {
    vehicle_id: u32,
    body_id: u32,
};

/// Stable cell identity: the tuple the game itself stores in net/state records.
const CellKey = struct {
    vehicle_id: u32,
    body_id: u32,
    x: i32,
    y: i32,
    z: i32,
};

const CellState = struct {
    temp_c: f32,
    /// Lumped heat capacity, J/K (mass split over the component's cells).
    capacity_j_k: f32,
    /// Environment temperature (air or fire) of the last step, C.
    t_env_c: f32,
    /// Last tick the parent body was enumerated (drives stale pruning).
    last_seen_ms: u64,
};

const BodyMeta = struct {
    last_seen_ms: u64,
    /// Interlace phase in `0..SIM_INTERLACE`.
    phase: u32,
    /// Ambient air temperature (°C) at the body from its last step, NaN until
    /// then. The scene median of these is the FLIR main view's ambient.
    air_c: f32,
    /// Air temperature at sea level under the body (terrain/water ambient in
    /// the composite: an aircraft's own air is colder than the ground's).
    ground_air_c: f32,
    /// Last hottest-cells log time.
    last_hot_log_ms: u64,
};

/// A component cell while a body grid is being built.
const RawCell = struct {
    x: i32,
    y: i32,
    z: i32,
    mass_kg: f32,
    /// Owning component pointer (key into the engine source snapshot).
    component: usize,
};

/// A cell of the body currently being stepped, with its texel index.
const CellSlot = struct {
    index: usize,
    x: i32,
    y: i32,
    z: i32,
    capacity_j_k: f32,
    key: CellKey,
    /// Real component temperature (C) from the plugin's engine sources.
    source_c: ?f32,
    /// Owning component pointer.
    component: usize,
};

/// A heat point radiating into nearby cells: world position, temperature and owner.
const HeatPoint = struct { pos: Vec3, temp_c: f32, owner: HeatOwner };

/// An engine source snapshot entry: temperature and world position.
const SourceSnap = struct { temp_c: f32, pos: Vec3 };

const Sim = struct {
    cells: fx.HashMap(CellKey, CellState) = .empty,
    bodies: fx.HashMap(BodyKey, BodyMeta) = .empty,
    next_version: u64 = 1,
    /// Body pointer -> last built grid, reused across ticks (allocations kept).
    staging: fx.HashMap(usize, BodyGrid) = .empty,
    /// Body pointers enumerated this tick (stale-grid/publish filter).
    seen: fx.HashSet(usize) = .empty,
    /// Body pointers stepped this tick: only these are copied into PUBLISHED.
    stepped: fx.HashSet(usize) = .empty,
    /// Scratch buffers, never reallocated per cell.
    pending: std.ArrayList(RawCell) = .empty,
    slots: std.ArrayList(CellSlot) = .empty,
    scratch: std.ArrayList(f32) = .empty,
    /// Burning voxels of all bodies in world space, rebuilt by each fire scan.
    fire: FireField = .{},
    /// Hot source cells per body (world point, temp, owner), refreshed when the body steps.
    heat_src: fx.HashMap(usize, std.ArrayList(HeatPoint)) = .empty,
    last_fire_scan_ms: u64 = 0,
    /// Bodies whose fire octree layout was already dumped / last fire log time.
    fire_dumped: fx.HashSet(usize) = .empty,
    /// Bodies whose raw fire counters (+0x1220 records / +0x1228 default cells) were nonzero last scan.
    fire_bodies_raw: u32 = 0,
    fire_log_ms: fx.HashMap(usize, u64) = .empty,
    /// Remaining grid bytes this tick may still allocate (see
    /// `MAX_GRID_BYTES`); staging already holds most of it across ticks.
    grid_budget: usize = MAX_GRID_BYTES,
    /// Cells of all bodies walked in the current tick (status line only).
    tick_cells: usize = 0,
    tick_phase: u32 = 0,
    next_phase: u32 = 0,
    /// Plugin clock (us) of the previous tick.
    last_tick_us: ?u64 = null,
    last_prune_ms: u64 = 0,
    /// This tick's fresh engine source temperatures, by component pointer.
    sources: fx.HashMap(usize, SourceSnap) = .empty,
    /// Components already logged as bound to a source.
    bound: fx.HashSet(usize) = .empty,

    const scanFires = SimMethods.scanFires;
    const beginTick = SimMethods.beginTick;
    const processBody = processBodyFn;
    const logHottest = logHottestFn;
    const publish = publishFn;
};

var SIM: win.Mutex(Sim) = .{ .value = .{} };
/// Grids of the current frame. The render thread only ever reads this map; the
/// sim thread copies in only the bodies it stepped this tick, so a body that is
/// not due keeps exactly the grid it last published.
var PUBLISHED: win.Mutex(fx.HashMap(usize, BodyGrid)) = .{ .value = .empty };

// ---------------------------------------------------------------------------
// One-shot diagnostic keys (verbose builds only, see `logging.logOnce`)
// ---------------------------------------------------------------------------

// `blocktemp` owns bits 0..8 of the shared one-shot word; `flir` owns 8..22.
const LOG_MATRIX: u32 = 1 << 0;
const LOG_CELLS: u32 = 1 << 1;
const LOG_SKIP: u32 = 1 << 3;
const LOG_SET: u32 = 1 << 4;
const LOG_BUDGET: u32 = 1 << 5;
const LOG_SRC_FAR: u32 = 1 << 6;
const LOG_PLUME_NO_AXIS: u32 = 1 << 2;
const LOG_PLUME: u32 = 1 << 7;

/// Interval of the "sim is running" line: enough to see a vehicle spawn, the
/// first probes and the world-root state in a log a player will paste.
const STATUS_INTERVAL_MS: u64 = 5000;
var STATUS_MS: Atomic(u64) = .init(0);
/// Set by the first tick that saw at least one vehicle: that tick logs
/// immediately instead of waiting out the interval (the first status line was
/// otherwise easy to miss in a short session).
var STATUS_FIRST_VEHICLE: Atomic(bool) = .init(false);

/// The first tick that sees a vehicle, then one line every ~5 s: what the scene
/// walk sees, how many burning voxels the last fire scan found and whether the
/// game's heat model has a validated world root. Without it a log that stops at the mesh map
/// cannot tell "sim never ran" from "sim ran and found nothing".
fn reportStatus(sim: *const Sim, stats: WalkStats) void {
    if (!verbose) return;
    const now_ms = state.nowMs();
    const first_vehicle = stats.vehicles >= 1 and !STATUS_FIRST_VEHICLE.swap(true, .monotonic);
    if (!first_vehicle and now_ms -| STATUS_MS.load(.monotonic) < STATUS_INTERVAL_MS) return;
    STATUS_MS.store(now_ms, .monotonic);
    const world_ok = heat.world() != null;
    logf("blocktemp status: vehicles={d} bodies={d} bodies_tracked={d} cells_tick={d} fire_voxels={d} fire_bodies_raw={d} cells_tracked={d} world_root={s} | tick {f} | ir.scan {f}", .{
        stats.vehicles,
        stats.bodies,
        sim.bodies.count(),
        sim.tick_cells,
        sim.fire.len,
        sim.fire_bodies_raw,
        sim.cells.count(),
        if (world_ok) "ok" else "none",
        TICK_COST.take(),
        SCAN_COST.take(),
    });
}

// ---------------------------------------------------------------------------
// Game-memory helpers
// ---------------------------------------------------------------------------

inline fn readableAt(address: usize, len: usize) bool {
    return memory.inPtrRange(address) and readable(address, len);
}

/// A game `c_sw_set`-style hash set: `{ T** items; u32 mod; u32 offset; u32 count }`.
pub const GameSet = struct {
    items: usize,
    modulo: u32,
    offset: u32,
    count_: u32,

    /// Pointer at logical index `i`; the physical slot is hash-scrambled
    /// (`(i + offset) % mod`), so `i` is never an id. Only used for 8-byte
    /// pointer sets; definition cell sets are indexed by the caller.
    pub fn item(self: GameSet, i: u32) usize {
        const index = (@as(usize, i) + self.offset) % self.modulo;
        return readUsizeAt(self.items + index * 8);
    }

    pub fn count(self: GameSet) u32 {
        return self.count_;
    }
};

/// Validate and snapshot a set header. `stride` is the entry size (8 for a
/// pointer set, 60 for a definition's voxel-cell set).
pub fn readSet(base: usize, count_limit: u32, stride: usize, mod_limit: u32) ?GameSet {
    if (!readableAt(base, 0x18)) return null;
    const items = readUsizeAt(base);
    const modulo = readU32At(base + 8);
    const offset = readU32At(base + 0xC);
    const count = readU32At(base + 0x10);
    if (!memory.inPtrRange(items) or modulo == 0 or modulo > mod_limit or count == 0 or count > modulo or count > count_limit) {
        return null;
    }
    if (!readable(items, @as(usize, modulo) * stride)) return null;
    return .{ .items = items, .modulo = modulo, .offset = offset, .count_ = count };
}

/// Row-major float4x4 (translation in row 3) that is finite and non-degenerate.
/// Body voxel-metres -> world as one f64 row-vector matrix: the physics world matrix
/// with its translation pre-shifted by -origin·basis.
fn bodyWorldMatrix(body: usize) ?[16]f64 {
    if (!readableAt(body + OFF_BODY_PHYSICS, 8) or !readableAt(body + OFF_BODY_ORIGIN, 12)) return null;
    const object = readUsizeAt(body + OFF_BODY_PHYSICS);
    if (object < PTR_MIN or !readableAt(object + OFF_PHYSICS_WORLD, 16 * 8)) return null;
    var m: [16]f64 = undefined;
    for (&m, 0..) |*value, i| value.* = readF64At(object + OFF_PHYSICS_WORLD + i * 8);
    const o = [3]f64{
        readF32At(body + OFF_BODY_ORIGIN),
        readF32At(body + OFF_BODY_ORIGIN + 4),
        readF32At(body + OFF_BODY_ORIGIN + 8),
    };
    for (0..3) |c| m[12 + c] -= o[0] * m[c] + o[1] * m[4 + c] + o[2] * m[8 + c];
    return m;
}

fn matrixOk(m: *const [16]f64) bool {
    for (m) |v| if (!std.math.isFinite(v)) return false;
    if (@abs(m[12]) > 1.0e6 or @abs(m[13]) > 1.0e6 or @abs(m[14]) > 1.0e6) return false;
    // The rotation/scale part must not collapse the voxel frame to a point.
    const row0 = m[0] * m[0] + m[1] * m[1] + m[2] * m[2];
    return row0 > 1.0e-6;
}

/// Voxel cell -> world metres: `voxel_m * M`, row-vector convention.
inline fn voxelToWorld(m: *const [16]f64, x: i32, y: i32, z: i32) Vec3 {
    const vx = util.f(x) * @as(f64, CELL_M);
    const vy = util.f(y) * @as(f64, CELL_M);
    const vz = util.f(z) * @as(f64, CELL_M);
    return .{
        vx * m[0] + vy * m[4] + vz * m[8] + m[12],
        vx * m[1] + vy * m[5] + vz * m[9] + m[13],
        vx * m[2] + vy * m[6] + vz * m[10] + m[14],
    };
}

/// The game's ambient base temperature (no heat sources).
fn airTemperatureAt(point: Vec3) f32 {
    const t = heat.baseAmbientAt(point);
    return if (std.math.isFinite(t)) std.math.clamp(t, MIN_TEMP_C, MAX_TEMP_C) else DEFAULT_AIR_C;
}

/// Texel index of a grid-local voxel: `x` fastest, then `y`, then `z` (the
/// layout `glTexImage3D(width=dims[0], height=dims[1], depth=dims[2])` and the
/// shader's `texelFetch(tex, voxel - grid_min)` read).
inline fn gridIndex(local: [3]usize, dims: [3]usize) usize {
    return local[2] * dims[0] * dims[1] + local[1] * dims[0] + local[0];
}

/// One jet exhaust plume in body-local metres.
pub const Plume = struct { start: [3]f32, dir: [3]f32, radius: f32, excess_c: f32 };

/// Hot-gas plumes of this body's jet exhausts in body-local metres (voxel *
/// 0.25, the block mesh's own space). The flow axis runs from the engine's
/// nearest other part (compressor or combustor) to the exhaust part, so no
/// part-rotation convention is needed.
fn jetPlumes(cells: []const RawCell, t_air: f32, out: *std.ArrayList(Plume)) void {
    out.clearRetainingCapacity();
    const Jet = struct { component: usize, part_type: u32, temp_c: f64 };
    var jets: std.ArrayList(Jet) = .empty;
    defer jets.deinit(gpa);
    {
        const parts = state.JET_PARTS.lock();
        defer state.JET_PARTS.unlock();
        if (parts.items.len == 0) return;
        for (parts.items) |e| jets.append(gpa, .{ .component = e.component, .part_type = e.part_type, .temp_c = e.ir_temp_c }) catch return;
    }
    // component -> (voxel sum, cells, part type, temp)
    const Part = struct { component: usize, sum: [3]f32, cells: u32, part_type: u32, temp_c: f32 };
    var parts: std.ArrayList(Part) = .empty;
    defer parts.deinit(gpa);
    for (cells) |raw| {
        const jet = for (jets.items) |j| {
            if (j.component == raw.component) break j;
        } else continue;
        const v = [3]f32{ util.f32_(raw.x), util.f32_(raw.y), util.f32_(raw.z) };
        for (parts.items) |*p| {
            if (p.component == raw.component) {
                for (0..3) |i| p.sum[i] += v[i];
                p.cells += 1;
                break;
            }
        } else {
            parts.append(gpa, .{ .component = raw.component, .sum = v, .cells = 1, .part_type = jet.part_type, .temp_c = @floatCast(jet.temp_c) }) catch return;
        }
    }
    const centre = struct {
        fn of(p: *const Part) [3]f32 {
            const n = util.f32_(p.cells);
            return .{ p.sum[0] / n * CELL_M, p.sum[1] / n * CELL_M, p.sum[2] / n * CELL_M };
        }
    }.of;
    var points: std.ArrayList([3]f32) = .empty;
    defer points.deinit(gpa);
    for (parts.items) |*ex| {
        if (ex.part_type != offsets.JET_EXHAUST_PART_TYPE) continue;
        const excess = ex.temp_c - t_air;
        if (!(excess > 50.0) or out.items.len >= 8) continue;
        const e = centre(ex);
        var nearest: ?struct { d: [3]f32, len: f32 } = null;
        for (parts.items) |*p| {
            if (p.part_type == offsets.JET_EXHAUST_PART_TYPE) continue;
            const c = centre(p);
            const d = [3]f32{ e[0] - c[0], e[1] - c[1], e[2] - c[2] };
            const len = @sqrt(d[0] * d[0] + d[1] * d[1] + d[2] * d[2]);
            if (!(len > 0.05 and len < 4.0)) continue;
            if (nearest == null or len < nearest.?.len) nearest = .{ .d = d, .len = len };
        }
        const near = nearest orelse {
            logOnce(LOG_PLUME_NO_AXIS, "blocktemp: jet exhaust with no other jet part within 4 m; no plume", .{});
            continue;
        };
        // A long exhaust (3x3x5) has its flow along its longest axis; the
        // neighbour part only picks the sign (twin engines side by side make
        // the nearest part sit diagonally).
        var d = [3]f32{ near.d[0] / near.len, near.d[1] / near.len, near.d[2] / near.len };
        points.clearRetainingCapacity();
        for (cells) |c| {
            if (c.component == ex.component) points.append(gpa, .{ util.f32_(c.x), util.f32_(c.y), util.f32_(c.z) }) catch {};
        }
        const pa = principalAxis(points.items);
        if (pa.spread > 1.5) {
            const s: f32 = if (pa.axis[0] * d[0] + pa.axis[1] * d[1] + pa.axis[2] * d[2] < 0.0) -1.0 else 1.0;
            d = .{ pa.axis[0] * s, pa.axis[1] * s, pa.axis[2] * s };
        }
        const len: f32 = 1.0;
        // Extent along the flow (cells): the nozzle is the cross-section, the
        // plume starts at the outlet face.
        var lo: f32 = std.math.floatMax(f32);
        var hi: f32 = -std.math.floatMax(f32);
        for (cells) |c| {
            if (c.component != ex.component) continue;
            const proj = util.f32_(c.x) * d[0] + util.f32_(c.y) * d[1] + util.f32_(c.z) * d[2];
            lo = util.fmin(lo, proj);
            hi = util.fmax(hi, proj);
        }
        const span = util.fmax(hi - lo + 1.0, 1.0);
        const start = [3]f32{
            e[0] + d[0] * 0.5 * span * CELL_M,
            e[1] + d[1] * 0.5 * span * CELL_M,
            e[2] + d[2] * 0.5 * span * CELL_M,
        };
        const radius = util.fclamp(0.5 * @sqrt(util.f32_(ex.cells) / span) * CELL_M, 0.15, 1.0);
        logOnce(LOG_PLUME, "blocktemp: first jet plume start=[{d}, {d}, {d}] m dir=({d:.2},{d:.2},{d:.2}) radius={d:.2} m excess={d:.0} C exhaust_cells={d}", .{
            start[0], start[1], start[2], d[0] / len, d[1] / len, d[2] / len, radius, excess, ex.cells,
        });
        out.append(gpa, .{ .start = start, .dir = .{ d[0] / len, d[1] / len, d[2] / len }, .radius = radius, .excess_c = excess }) catch return;
    }
}

/// Unit longest axis of a point set and how much longer it is than the next
/// (ratio of variances); power iteration on the 3x3 covariance.
fn principalAxis(pts: []const [3]f32) struct { axis: [3]f32, spread: f32 } {
    if (pts.len < 2) return .{ .axis = .{ 0, 0, 1 }, .spread = 0 };
    const n = util.f32_(pts.len);
    var m = [3]f32{ 0, 0, 0 };
    for (0..3) |i| {
        for (pts) |p| m[i] += p[i];
        m[i] /= n;
    }
    var c = [3][3]f32{ .{ 0, 0, 0 }, .{ 0, 0, 0 }, .{ 0, 0, 0 } };
    for (pts) |p| {
        for (0..3) |i| {
            for (0..3) |j| c[i][j] += (p[i] - m[i]) * (p[j] - m[j]) / n;
        }
    }
    var v = [3]f32{ 0.577, 0.577, 0.577 };
    var lambda: f32 = 0.0;
    for (0..32) |_| {
        var w: [3]f32 = undefined;
        for (0..3) |i| w[i] = c[i][0] * v[0] + c[i][1] * v[1] + c[i][2] * v[2];
        lambda = @sqrt(w[0] * w[0] + w[1] * w[1] + w[2] * w[2]);
        if (lambda < 1e-9) return .{ .axis = .{ 0, 0, 1 }, .spread = 0 };
        v = .{ w[0] / lambda, w[1] / lambda, w[2] / lambda };
    }
    // Variance across the axis: trace minus the axis' share, split two ways.
    const rest = util.fmax((c[0][0] + c[1][1] + c[2][2] - lambda) / 2.0, 1e-6);
    return .{ .axis = v, .spread = lambda / rest };
}

/// Per-body position, published on the body's simulation turn, so a seeker
/// can ignore heat sources on its own vehicle.
const BodyIr = struct {
    stamp_ms: u64,
    /// SCENE_TICKS at publish: game time, unlike wall-clock ms, which bunches
    /// when several sim ticks run in one frame.
    stamp_tick: u64,
    vehicle_id: u32,
    /// Grid-centre world position, its velocity (m/s) and bounding-sphere radius.
    center: Vec3,
    vel: Vec3,
    sphere_r: f64,

    /// A body is published once per interlace turn (~4 ticks), so a fast
    /// aircraft would be aimed at where it was; extrapolate to the current tick.
    fn lead(self: BodyIr, p: Vec3) Vec3 {
        const now = SCENE_TICKS.load(.monotonic);
        const t = util.fmin(util.f(now -| self.stamp_tick) * TICK_S, 0.25);
        return .{ p[0] + self.vel[0] * t, p[1] + self.vel[1] * t, p[2] + self.vel[2] * t };
    }
};
const BodyIrEntry = struct { body: usize, ir: BodyIr };
var BODY_IR: win.Mutex(std.ArrayList(BodyIrEntry)) = .{ .value = .empty };
const BODY_IR_STALE_MS: u64 = 2_000;
/// Cells and sources below this world height are under the sea (y = 0), and
/// water is opaque to IR.
pub const SEA_LEVEL_IR_M: f64 = -0.5;

/// Scene ticks seen by blocktemp (60 per game second).
var SCENE_TICKS: Atomic(u64) = .init(0);
const TICK_S: f64 = 1.0 / 60.0;

/// Seeker components that have scanned recently: the vehicle id once a body
/// step has seen it, and the last scan ms. Lets a seeker ignore its own vehicle.
const Seeker = struct { component: usize, vehicle_id: ?u32, seen_ms: u64 };
var SEEKERS: win.Mutex(std.ArrayList(Seeker)) = .{ .value = .empty };
const SEEKER_STALE_MS: u64 = 10_000;

fn noteSeekerVehicle(component: usize, vehicle_id: u32) void {
    const all = SEEKERS.lock();
    defer SEEKERS.unlock();
    for (all.items) |*e| {
        if (e.component == component) {
            e.vehicle_id = vehicle_id;
            return;
        }
    }
}

/// Register `component` as a live seeker and return its vehicle id, once a
/// body step has found the component among that vehicle's cells.
pub fn seekerVehicle(component: usize) ?u32 {
    const now = state.nowMs();
    const all = SEEKERS.lock();
    defer SEEKERS.unlock();
    var keep: usize = 0;
    for (all.items) |e| {
        if (now -| e.seen_ms < SEEKER_STALE_MS) {
            all.items[keep] = e;
            keep += 1;
        }
    }
    all.shrinkRetainingCapacity(keep);
    for (all.items) |*e| {
        if (e.component == component) {
            e.seen_ms = now;
            return e.vehicle_id;
        }
    }
    if (all.items.len < 64) all.append(gpa, .{ .component = component, .vehicle_id = null, .seen_ms = now }) catch {};
    return null;
}

fn publishBodyIr(body: usize, entry: BodyIr) void {
    var e = entry;
    const all = BODY_IR.lock();
    defer BODY_IR.unlock();
    for (all.items) |prev| {
        if (prev.body != body) continue;
        // Wall-clock dt was the bug: two publishes a few ms apart (bunched
        // ticks) turned one tick of motion into thousands of m/s of lead.
        const dt = util.f(e.stamp_tick -| prev.ir.stamp_tick) * TICK_S;
        e.vel = prev.ir.vel;
        if (dt > 0.0 and dt < 1.0) {
            const v = Vec3{
                (e.center[0] - prev.ir.center[0]) / dt,
                (e.center[1] - prev.ir.center[1]) / dt,
                (e.center[2] - prev.ir.center[2]) / dt,
            };
            // A grid rebuild (damage, detach) moves the centre without motion.
            if (@sqrt(v[0] * v[0] + v[1] * v[1] + v[2] * v[2]) < 1500.0) e.vel = v;
        }
        break;
    }
    const now = e.stamp_ms;
    var keep: usize = 0;
    for (all.items) |p| {
        if (p.body != body and now -| p.ir.stamp_ms < BODY_IR_STALE_MS) {
            all.items[keep] = p;
            keep += 1;
        }
    }
    all.shrinkRetainingCapacity(keep);
    const finite = std.math.isFinite(e.vel[0]) and std.math.isFinite(e.vel[1]) and std.math.isFinite(e.vel[2]);
    if (finite and all.items.len < 512) all.append(gpa, .{ .body = body, .ir = e }) catch {};
}

/// A body's bounding sphere in world space.
pub const Sphere = struct { center: Vec3, radius: f64 };

/// Bounding spheres of every body of `vehicle_id`, so a seeker can ignore heat
/// sources on its own vehicle. The caller owns the list.
pub fn vehicleSpheres(vehicle_id: u32) std.ArrayList(Sphere) {
    const now = state.nowMs();
    var out: std.ArrayList(Sphere) = .empty;
    const all = BODY_IR.lock();
    defer BODY_IR.unlock();
    for (all.items) |p| {
        if (p.ir.vehicle_id == vehicle_id and now -| p.ir.stamp_ms < BODY_IR_STALE_MS) {
            out.append(gpa, .{ .center = p.ir.lead(p.ir.center), .radius = p.ir.sphere_r }) catch break;
        }
    }
    return out;
}

/// Heat-point temperature of an engine exhaust outlet (pipe wall, not gas).
const EXHAUST_PIPE_C: f32 = 250.0;

/// Daylight factor `0..1` from the game's sun: its normalized height is the
/// sine of the sun's elevation.
fn daylightFactor() f32 {
    // The game's own sun (the clock-driven rotation the renderer uses). The
    // per-tick sun-sprite hook never fired in recent runs, which left every
    // vehicle with zero solar gain.
    if (sun.gameSun()) |s| return std.math.clamp(@as(f32, @floatCast(s.dir[1])), 0.0, 1.0);
    if (!state.SUN_DIR_VALID.load(.monotonic)) return 0.0;
    const dir = blk: {
        const g = state.SUN_DIR.lock();
        defer state.SUN_DIR.unlock();
        break :blk g.*;
    };
    const len = @sqrt(dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2]);
    if (!std.math.isFinite(len) or len <= 1.0e-6) return 0.0;
    return @floatCast(util.fclamp(dir[1] / len, 0.0, 1.0));
}

// ---------------------------------------------------------------------------
// Thermal step (pure; the unit tests drive this directly)
// ---------------------------------------------------------------------------

const StepInputs = struct {
    capacity_j_k: f32,
    dt_s: f32,
    t_env_c: f32,
    t_air_c: f32,
    /// Conductance to `T_env`, W/K.
    h_env_w_k: f32,
    /// Conductance to ambient air, W/K.
    h_air_w_k: f32,
    /// Conductance to each present face neighbour, W/K.
    k_cond_w_k: f32,
    neighbour_count: u32,
    /// Sum of the present neighbours' temperatures, C.
    neighbour_sum_c: f32,
    solar_w: f32,
    /// Real temperature of the cell's component, if it is a live source.
    source_c: ?f32,
};

/// One explicit-Euler step. The step is shortened when `dt * conductance / C`
/// would exceed `MAX_STEP_FRACTION`, so a tiny/heavy-mass cell can never
/// overshoot its target.
fn stepTemperature(t_c: f32, input: *const StepInputs) f32 {
    const air = util.fclamp(if (std.math.isFinite(input.t_air_c)) input.t_air_c else DEFAULT_AIR_C, MIN_TEMP_C, MAX_TEMP_C);
    if (!std.math.isFinite(t_c) or !std.math.isFinite(input.capacity_j_k)) return air;
    const capacity = util.fmax(input.capacity_j_k, MIN_CAPACITY_J_K);
    const neighbours = util.f32_(input.neighbour_count);
    var src_t: f32 = 0.0;
    var h_src: f32 = 0.0;
    if (input.source_c) |t| {
        if (std.math.isFinite(t)) {
            src_t = std.math.clamp(t, MIN_TEMP_C, MAX_TEMP_C);
            h_src = capacity / SOURCE_TAU_S;
        }
    }
    const conductance = input.h_env_w_k + input.h_air_w_k + input.k_cond_w_k * neighbours + h_src;
    var dt = if (conductance > 0.0) util.fmin(input.dt_s, MAX_STEP_FRACTION * capacity / conductance) else input.dt_s;
    dt = if (std.math.isFinite(dt)) util.fmax(dt, 0.0) else 0.0;
    const heat_w = input.h_env_w_k * (input.t_env_c - t_c) +
        input.h_air_w_k * (air - t_c) +
        input.k_cond_w_k * (input.neighbour_sum_c - neighbours * t_c) +
        input.solar_w +
        h_src * (src_t - t_c);
    const next = t_c + dt / capacity * heat_w;
    return if (std.math.isFinite(next)) std.math.clamp(next, MIN_TEMP_C, MAX_TEMP_C) else air;
}

// ---------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------

/// One component definition's cell data. A vehicle reuses a few hundred
/// definitions across thousands of blocks, and every miss costs a handful of
/// `readable` (VirtualQuery) calls, so it is read once per body walk.
const DefCells = struct {
    cells: ?GameSet,
    /// Mass of one voxel cell, kg (definition mass split over the cell set).
    per_cell_kg: f32,
};

/// Read a definition's cell set and split its mass over the cells.
fn readDefinition(definition: usize) DefCells {
    const mass: f32 = blk: {
        if (!readableAt(definition, OFF_DEF_MASS + 4)) break :blk 0.0;
        const m = readF32At(definition + OFF_DEF_MASS);
        break :blk if (std.math.isFinite(m) and m > 0.0) m else 0.0;
    };
    const cells = readSet(definition + OFF_DEF_CELLS, MAX_CELLS_PER_COMPONENT, CELL_ENTRY_BYTES, MAX_SET_MOD);
    const per_cell_kg: f32 = if (cells) |set| blk: {
        const per_cell = if (mass > 0.0) mass / util.f32_(set.count_) else FALLBACK_MASS_KG_PER_CELL;
        break :blk if (std.math.isFinite(per_cell) and per_cell > 0.0) per_cell else FALLBACK_MASS_KG_PER_CELL;
    } else if (mass > 0.0)
        // No readable cell set (or a usable mass without one): one voxel.
        mass
    else
        FALLBACK_MASS_KG_PER_CELL;
    return .{ .cells = cells, .per_cell_kg = per_cell_kg };
}

fn outOfRange(v: i32) bool {
    return @abs(v) > MAX_VOXEL_COORD;
}

var MULTI_VOXEL_SEEN: win.Mutex(std.ArrayList(usize)) = .{ .value = .empty };

/// Read one body's components into `out` as voxel cells, splitting each
/// component's mass over its cells. `ok` is false only when the component set
/// itself is unreadable.
fn collectCells(body_ptr: usize, out: *std.ArrayList(RawCell)) struct { ok: bool, components: u32 } {
    const components = readSet(body_ptr + OFF_BODY_COMPONENTS, MAX_COMPONENTS_PER_BODY, 8, MAX_SET_MOD) orelse
        return .{ .ok = false, .components = 0 };
    var components_read: u32 = 0;
    var defs: fx.HashMap(usize, DefCells) = .empty;
    defer defs.deinit(gpa);
    // The cap counts VISITED cells, not pushed ones: a cell whose voxel offset
    // is out of range is skipped without growing `out`, so `out.len` alone
    // would let a garbage set drive ~1e9 iterations per body.
    var visited = out.items.len;
    var i: u32 = 0;
    while (i < components.count_) : (i += 1) {
        if (visited >= MAX_CELLS_PER_BODY) break;
        const component = components.item(i);
        if (!readableAt(component, OFF_COMP_DEF + 8)) continue;
        components_read += 1;
        const definition = readUsizeAt(component + OFF_COMP_DEF);
        const origin = [3]i32{
            readI32At(component + OFF_COMP_VOXEL),
            readI32At(component + OFF_COMP_VOXEL + 4),
            readI32At(component + OFF_COMP_VOXEL + 8),
        };
        if (outOfRange(origin[0]) or outOfRange(origin[1]) or outOfRange(origin[2])) continue;

        // Voxel-space rotation: axis permutations only; anything else means the
        // pointer was not the component we expected.
        var rotation = [9]i32{ 1, 0, 0, 0, 1, 0, 0, 0, 1 };
        var rotation_ok = false;
        if (readableAt(component + OFF_COMP_ROT, 9 * 4)) {
            rotation_ok = true;
            for (&rotation, 0..) |*slot, k| {
                const value = readI32At(component + OFF_COMP_ROT + k * 4);
                if (value < -1 or value > 1) {
                    rotation_ok = false;
                    break;
                }
                slot.* = value;
            }
        }
        // A voxel rotation is a signed permutation: one +-1 per row and column.
        // Anything else (a zero row) would collapse a multi-voxel part's cells
        // onto a line - e.g. only the middle row of a 3x3 radiator.
        if (rotation_ok) {
            for (0..3) |r| {
                var row_n: usize = 0;
                var col_n: usize = 0;
                for (0..3) |c| {
                    if (rotation[r * 3 + c] != 0) row_n += 1;
                    if (rotation[c * 3 + r] != 0) col_n += 1;
                }
                if (row_n != 1 or col_n != 1) rotation_ok = false;
            }
        }
        if (!rotation_ok) rotation = .{ 1, 0, 0, 0, 1, 0, 0, 0, 1 };

        const def = defs.get(definition) orelse blk: {
            const d = readDefinition(definition);
            defs.put(gpa, definition, d) catch {};
            break :blk d;
        };

        if (def.cells) |set| {
            const first_index = set.offset % set.modulo;
            const first_entry = set.items + @as(usize, first_index) * CELL_ENTRY_BYTES;
            logOnce(LOG_SET, "blocktemp: first cell set def=0x{x} count={d} mod={d} offset={d} origin=({any}) rotation={any} first_offset=({d},{d},{d})", .{
                definition,             set.count_,                 set.modulo,                 set.offset, origin, rotation,
                readI32At(first_entry), readI32At(first_entry + 4), readI32At(first_entry + 8),
            });
            if (verbose and set.count_ > 1) logMultiVoxel(definition, component, set, origin, rotation, rotation_ok);
            const per_cell = def.per_cell_kg;
            var cell_index: u32 = 0;
            while (cell_index < set.count_) : (cell_index += 1) {
                if (visited >= MAX_CELLS_PER_BODY) break;
                visited += 1;
                const index = (@as(usize, cell_index) + set.offset) % set.modulo;
                const entry = set.items + index * CELL_ENTRY_BYTES;
                const offset = [3]i32{ readI32At(entry), readI32At(entry + 4), readI32At(entry + 8) };
                // Reject out-of-range offsets BEFORE rotating: the rotation
                // terms are `i32 * offset`, and an offset near minInt(i32)
                // would overflow.
                if (outOfRange(offset[0]) or outOfRange(offset[1]) or outOfRange(offset[2])) continue;
                const rotated = [3]i32{
                    rotation[0] * offset[0] + rotation[1] * offset[1] + rotation[2] * offset[2],
                    rotation[3] * offset[0] + rotation[4] * offset[1] + rotation[5] * offset[2],
                    rotation[6] * offset[0] + rotation[7] * offset[1] + rotation[8] * offset[2],
                };
                const x = origin[0] +| rotated[0];
                const y = origin[1] +| rotated[1];
                const z = origin[2] +| rotated[2];
                if (outOfRange(x) or outOfRange(y) or outOfRange(z)) continue;
                out.append(gpa, .{ .x = x, .y = y, .z = z, .mass_kg = per_cell, .component = component }) catch break;
            }
        } else {
            // No readable cell set: treat the component as one voxel. This
            // keeps plain 1x1x1 blocks working even if their definition set
            // layout ever changes.
            visited += 1;
            out.append(gpa, .{ .x = origin[0], .y = origin[1], .z = origin[2], .mass_kg = def.per_cell_kg, .component = component }) catch {};
        }
    }
    return .{ .ok = true, .components = components_read };
}

/// Multi-voxel parts are where rotation matters: one line per definition
/// (first 24) to check cells against the part.
fn logMultiVoxel(definition: usize, component: usize, set: GameSet, origin: [3]i32, rotation: [9]i32, rotation_ok: bool) void {
    {
        const seen = MULTI_VOXEL_SEEN.lock();
        defer MULTI_VOXEL_SEEN.unlock();
        if (seen.items.len >= 24 or std.mem.indexOfScalar(usize, seen.items, definition) != null) return;
        seen.append(gpa, definition) catch return;
    }
    var cells: [12][3]i32 = undefined;
    const n = @min(set.count_, 12);
    for (0..n) |k| {
        const e = set.items + ((k + set.offset) % set.modulo) * CELL_ENTRY_BYTES;
        cells[k] = .{ readI32At(e), readI32At(e + 4), readI32At(e + 8) };
    }
    logf("blocktemp: multi-voxel part def=0x{x} comp=0x{x} count={d} origin={any} rotation={any} (read_ok={}) cells={any}", .{
        definition, component, set.count_, origin, rotation, rotation_ok, cells[0..n],
    });
}

/// Inclusive voxel bounds, or null when the body exceeds the grid caps.
fn gridBounds(cells: []const RawCell) ?struct { min: [3]i32, dims: [3]u32, total: usize } {
    if (cells.len == 0) return null;
    var min = [3]i32{ std.math.maxInt(i32), std.math.maxInt(i32), std.math.maxInt(i32) };
    var max = [3]i32{ std.math.minInt(i32), std.math.minInt(i32), std.math.minInt(i32) };
    for (cells) |cell| {
        const coordinates = [3]i32{ cell.x, cell.y, cell.z };
        for (0..3) |axis| {
            min[axis] = @min(min[axis], coordinates[axis]);
            max[axis] = @max(max[axis], coordinates[axis]);
        }
    }
    var dims = [3]u32{ 0, 0, 0 };
    var total: usize = 1;
    for (0..3) |axis| {
        const extent: i64 = @as(i64, max[axis]) - min[axis] + 1;
        if (extent <= 0 or extent > MAX_GRID_DIM) return null;
        dims[axis] = @intCast(extent);
        total = std.math.mul(usize, total, @intCast(extent)) catch return null;
    }
    if (total == 0 or total > MAX_CELLS_PER_BODY) return null;
    return .{ .min = min, .dims = dims, .total = total };
}

// ---------------------------------------------------------------------------
// Tick
// ---------------------------------------------------------------------------

/// Walk the scene's vehicles and their bodies, calling `f(ctx, vehicle_id, body)`.
///
/// The one copy of the scene -> vehicles -> bodies container walk (the sim and
/// the render side's mesh map both use it): every level is `readable`-guarded
/// and clamped, and a body is only yielded when its id field is readable. `f`
/// returns false to stop the walk early.
pub fn forEachBody(scene: usize, ctx: anytype, comptime f: fn (@TypeOf(ctx), u32, usize) bool) WalkStats {
    var stats: WalkStats = .{};
    if (scene < PTR_MIN or !readable(scene + offsets.SCENE_VEHICLE_CONTAINER, 0x18)) return stats;
    const container = scene + offsets.SCENE_VEHICLE_CONTAINER;
    const records = readSet(container + offsets.SCENE_VEHICLE_SET, MAX_VEHICLES, 8, MAX_SET_MOD) orelse return stats;
    var bodies_seen: usize = 0;
    var i: u32 = 0;
    while (i < records.count_) : (i += 1) {
        const record = records.item(i);
        if (!readableAt(record, 0x14)) continue;
        if (memory.peek(u8, record) != 0) continue; // removed/inactive record
        if (readU32At(record + OFF_REC_TYPE) != REC_TYPE_VEHICLE) continue;
        const vehicle_id = readU32At(record + OFF_REC_VEHICLE_ID);
        const vehicle = readUsizeAt(record + OFF_REC_VEHICLE);
        if (!readableAt(vehicle, 0x18)) continue;
        const bodies = readSet(vehicle + OFF_VEHICLE_BODIES, MAX_BODIES_PER_VEHICLE, 8, MAX_SET_MOD) orelse continue;
        stats.vehicles += 1;
        var j: u32 = 0;
        while (j < bodies.count_) : (j += 1) {
            if (bodies_seen >= MAX_BODIES_TOTAL) return stats;
            const body = bodies.item(j);
            if (!readableAt(body, OFF_BODY_ID + 4)) continue;
            bodies_seen += 1;
            stats.bodies += 1;
            if (!f(ctx, vehicle_id, body)) return stats;
        }
    }
    return stats;
}

const TickCtx = struct {
    sim: *Sim,
    now_ms: u64,
    dt: f32,
    sun: f32,
    tick_phase: u32,

    fn visit(self: *const TickCtx, vehicle_id: u32, body: usize) bool {
        self.sim.processBody(body, vehicle_id, self.now_ms, self.dt, self.sun, self.tick_phase);
        return true;
    }
};

fn tickInner(scene: usize) void {
    fault.enter(stage.BLOCKTEMP_TICK);
    defer fault.leave();
    _ = SCENE_TICKS.fetchAdd(1, .monotonic);
    const now_ms = state.nowMs();
    const now_us = state.nowUs();
    const sun_factor = daylightFactor();

    // Snapshot fresh engine sources first; no lock is held during the sim.
    var fresh: fx.HashMap(usize, SourceSnap) = .empty;
    {
        const pending = state.EMITTER_PENDING.lock();
        defer state.EMITTER_PENDING.unlock();
        for (pending.items) |e| {
            if (now_ms -| e.seen_ms > SOURCE_FRESH_MS) continue;
            if (!(std.math.isFinite(e.temp_c) and e.temp_c > 0.0)) continue;
            fresh.put(gpa, e.key, .{ .temp_c = @floatCast(e.temp_c), .pos = .{ e.x, e.y, e.z } }) catch {};
        }
    }

    const sim = SIM.lock();
    defer SIM.unlock();
    retainKeys(&sim.bound, &fresh);
    sim.sources.deinit(gpa);
    sim.sources = fresh;
    const begun = sim.beginTick(now_us);
    sim.seen.clearRetainingCapacity();
    sim.stepped.clearRetainingCapacity();
    sim.scanFires(scene, now_ms);

    fault.set(stage.BLOCKTEMP_ENUMERATE);
    const ctx: TickCtx = .{ .sim = sim, .now_ms = now_ms, .dt = begun.dt, .sun = sun_factor, .tick_phase = begun.phase };
    const stats = forEachBody(scene, &ctx, TickCtx.visit);

    fault.set(stage.BLOCKTEMP_PUBLISH);
    sim.publish(now_ms);
    reportStatus(sim, stats);
}

/// Keep only the keys of `set` that `keep` also has.
fn retainKeys(set: anytype, keep: anytype) void {
    var drop: std.ArrayList(usize) = .empty;
    defer drop.deinit(gpa);
    var it = set.keyIterator();
    while (it.next()) |key| {
        if (!keep.contains(key.*)) drop.append(gpa, key.*) catch {};
    }
    for (drop.items) |key| _ = set.remove(key);
}

// ---------------------------------------------------------------------------
// Fire field: burning voxels in world space
// ---------------------------------------------------------------------------

/// `T_env` for a cell at distance `d` metres from the nearest burning voxel.
fn fireTempAt(d: f64, t_air: f32) f32 {
    return heatTempAt(d, FIRE_TEMP_C, t_air);
}

/// Point at `point_c`: full within `FIRE_FULL_M`, linear to `t_air` at `FIRE_FADE_M`.
fn heatTempAt(d: f64, point_c: f32, t_air: f32) f32 {
    const k: f32 = @floatCast(util.fclamp((FIRE_FADE_M - d) / (FIRE_FADE_M - FIRE_FULL_M), 0.0, 1.0));
    return util.fmax(t_air, t_air + (point_c - t_air) * k);
}

/// Owner of a heat point (body pointer + cell), so a source cell can skip itself.
const HeatOwner = struct { body: usize, cell: [3]i32 };
/// Owner voxel x of a conducted heat point: it heats every body but its own.
const BODY_WIDE_OWNER: i32 = std.math.minInt(i32);
const CONDUCTED_POINT_EXCESS_C: f32 = 15.0;
const MAX_CONDUCTED_POINTS: usize = 2048;

const FieldPoint = struct { pos: Vec3, temp_c: f32, owner: ?HeatOwner };

/// World-space heat points (burning voxels and hot source cells) in a spatial hash.
const FireField = struct {
    buckets: fx.HashMap([3]i32, std.ArrayList(FieldPoint)) = .empty,
    len: usize = 0,
    /// World bounds of all points (valid when `len > 0`), for a per-body reject.
    lo: Vec3 = .{ 0, 0, 0 },
    hi: Vec3 = .{ 0, 0, 0 },

    fn deinit(self: *FireField) void {
        var it = self.buckets.valueIterator();
        while (it.next()) |points| points.deinit(gpa);
        self.buckets.deinit(gpa);
        self.* = .{};
    }

    fn bucket(p: Vec3) [3]i32 {
        return .{
            util.f2i(i32, @floor(p[0] / FIRE_BUCKET_M)),
            util.f2i(i32, @floor(p[1] / FIRE_BUCKET_M)),
            util.f2i(i32, @floor(p[2] / FIRE_BUCKET_M)),
        };
    }

    fn insert(self: *FireField, p: Vec3, temp_c: f32, owner: ?HeatOwner) void {
        if (self.len >= MAX_HEAT_POINTS or !std.math.isFinite(temp_c) or
            !(std.math.isFinite(p[0]) and std.math.isFinite(p[1]) and std.math.isFinite(p[2])) or
            @abs(p[0]) > 1.0e6 or @abs(p[1]) > 1.0e6 or @abs(p[2]) > 1.0e6)
        {
            return;
        }
        const entry = self.buckets.getOrPut(gpa, bucket(p)) catch return;
        if (!entry.found_existing) entry.value_ptr.* = .empty;
        entry.value_ptr.append(gpa, .{ .pos = p, .temp_c = temp_c, .owner = owner }) catch return;
        if (self.len == 0) {
            self.lo = p;
            self.hi = p;
        } else {
            for (0..3) |i| {
                self.lo[i] = util.fmin(self.lo[i], p[i]);
                self.hi[i] = util.fmax(self.hi[i], p[i]);
            }
        }
        self.len += 1;
    }

    /// True when a box (world) comes within `FIRE_FADE_M` of any point's bounds.
    fn nearBox(self: *const FireField, lo: Vec3, hi: Vec3) bool {
        const r = FIRE_FADE_M;
        return self.len > 0 and
            lo[0] <= self.hi[0] + r and hi[0] >= self.lo[0] - r and
            lo[1] <= self.hi[1] + r and hi[1] >= self.lo[1] - r and
            lo[2] <= self.hi[2] + r and hi[2] >= self.lo[2] - r;
    }

    /// Hottest contribution within `FIRE_FADE_M`, never below `t_air`; the
    /// point owned by `own` is ignored.
    fn envTemp(self: *const FireField, p: Vec3, t_air: f32, own: ?HeatOwner) f32 {
        const b = bucket(p);
        var best = t_air;
        var dx: i32 = -1;
        while (dx <= 1) : (dx += 1) {
            var dy: i32 = -1;
            while (dy <= 1) : (dy += 1) {
                var dz: i32 = -1;
                while (dz <= 1) : (dz += 1) {
                    const points = self.buckets.get(.{ b[0] +% dx, b[1] +% dy, b[2] +% dz }) orelse continue;
                    for (points.items) |q| {
                        if (own) |w| {
                            if (q.owner) |o| {
                                if (std.meta.eql(o, w)) continue;
                                if (o.cell[0] == BODY_WIDE_OWNER and o.body == w.body) continue;
                            }
                        }
                        const d = @sqrt(util.sq(p[0] - q.pos[0]) + util.sq(p[1] - q.pos[1]) + util.sq(p[2] - q.pos[2]));
                        if (d < FIRE_FADE_M) best = util.fmax(best, heatTempAt(d, q.temp_c, t_air));
                    }
                }
            }
        }
        return best;
    }
};

/// One burning cell read from a body's fire octree.
const FireVoxel = struct {
    pos: [3]i32,
    active: bool,
    /// Decided by the slot vector (true) or by mere presence in the octree.
    by_slots: bool,
};

const FireScan = struct {
    records: u32 = 0,
    cells_field: i64 = 0,
    leaves: u32 = 0,
    rejected: u32 = 0,
    inner: u32 = 0,
    by_slots: u32 = 0,
    by_presence: u32 = 0,
};

inline fn alignedPtr(p: usize) bool {
    return p & 7 == 0 and memory.inPtrRange(p);
}

/// Is any slot of this leaf active? Null if the slot vector is unreadable/empty.
fn leafActive(leaf: usize) ?bool {
    const count: usize = readU32At(leaf + OFF_LEAF_COUNT);
    const cap: usize = readU32At(leaf + OFF_LEAF_CAP);
    const start: usize = readU32At(leaf + OFF_LEAF_START);
    const items = readUsizeAt(leaf + OFF_LEAF_SLOTS);
    if (count == 0 or count > 64 or cap == 0 or cap > 4096 or count > cap or !alignedPtr(items)) return null;
    if (!readable(items, cap * LEAF_SLOT_BYTES)) return null;
    var any = false;
    for (0..count) |i| {
        const idx = (start + i) % cap;
        any = any or memory.peek(u8, items + idx * LEAF_SLOT_BYTES + OFF_SLOT_ACTIVE) != 0;
    }
    return any;
}

/// Raw leaf dump lines (verbose builds), owned strings.
const DumpLines = std.ArrayList([]u8);

fn freeDumpLines(lines: *DumpLines) void {
    for (lines.items) |line| gpa.free(line);
    lines.deinit(gpa);
}

/// Walk a body's fire records (`body+0x1218` x `body+0x1220`, then the inline
/// default record at `body+0x1228`). Octree per record: 8 root slots at +0x08,
/// each an octant of `[-512,512)^3`; nodes halve until size 2, whose slots point
/// at 8-byte boxes holding the leaf pointer. A leaf's int3 cell (+0x18) must lie
/// in the cube its path describes. `dump` receives raw leaf bytes (verbose).
fn collectFireVoxels(body: usize, budget: *u32, out: *std.ArrayList(FireVoxel), dump: ?*DumpLines) FireScan {
    var scan: FireScan = .{};
    var recs_buf: [MAX_FIRE_RECORDS + 1]usize = undefined;
    var recs_n: usize = 0;
    if (readableAt(body + OFF_BODY_FIRES, 8) and readableAt(body + OFF_BODY_FIRE_COUNT, 4)) {
        const base = readUsizeAt(body + OFF_BODY_FIRES);
        const count = readU32At(body + OFF_BODY_FIRE_COUNT);
        if (count != 0 and count <= MAX_FIRE_RECORDS and alignedPtr(base) and readable(base, @as(usize, count) * FIRE_RECORD_BYTES)) {
            for (0..count) |r| {
                recs_buf[recs_n] = base + r * FIRE_RECORD_BYTES;
                recs_n += 1;
            }
        }
    }
    if (readableAt(body + OFF_BODY_DEFAULT_FIRE, FIRE_RECORD_BYTES)) {
        recs_buf[recs_n] = body + OFF_BODY_DEFAULT_FIRE;
        recs_n += 1;
    }
    // (node, cube min, cube edge)
    const Node = struct { node: usize, min: [3]i32, size: i32 };
    var stack: std.ArrayList(Node) = .empty;
    defer stack.deinit(gpa);
    for (recs_buf[0..recs_n]) |rec| {
        const cells = readI32At(rec + OFF_FIRE_CELLS);
        if (cells == 0) continue;
        scan.records += 1;
        scan.cells_field += cells;
        for (0..8) |o| {
            const root = readUsizeAt(rec + OFF_FIRE_ROOTS + o * 8);
            if (root == 0) continue;
            const half = FIRE_HALF;
            const min = [3]i32{
                -half + (if (o & 1 != 0) half else 0),
                -half + (if (o & 2 != 0) half else 0),
                -half + (if (o & 4 != 0) half else 0),
            };
            stack.append(gpa, .{ .node = root, .min = min, .size = half }) catch return scan;
            while (stack.pop()) |top| {
                if (budget.* == 0 or out.items.len >= MAX_FIRE_VOXELS) return scan;
                budget.* -= 1;
                if (!alignedPtr(top.node) or !readable(top.node, NODE_BYTES)) {
                    scan.rejected += 1;
                    continue;
                }
                scan.inner += 1;
                const h = @divTrunc(top.size, 2);
                for (0..8) |c| {
                    const child = readUsizeAt(top.node + c * 8);
                    if (child == 0) continue;
                    const cm = [3]i32{
                        top.min[0] + (if (c & 1 != 0) h else 0),
                        top.min[1] + (if (c & 2 != 0) h else 0),
                        top.min[2] + (if (c & 4 != 0) h else 0),
                    };
                    if (top.size > 2) {
                        stack.append(gpa, .{ .node = child, .min = cm, .size = h }) catch return scan;
                        continue;
                    }
                    // size == 2: child is a box holding the leaf pointer.
                    if (budget.* == 0) return scan;
                    budget.* -= 1;
                    if (!alignedPtr(child) or !readable(child, 8)) {
                        scan.rejected += 1;
                        continue;
                    }
                    const leaf = readUsizeAt(child);
                    if (!alignedPtr(leaf) or !readable(leaf, 0x28)) {
                        scan.rejected += 1;
                        continue;
                    }
                    const pos = [3]i32{
                        readI32At(leaf + OFF_NODE_POS),
                        readI32At(leaf + OFF_NODE_POS + 4),
                        readI32At(leaf + OFF_NODE_POS + 8),
                    };
                    var inside = true;
                    for (0..3) |i| {
                        if (!(pos[i] >= cm[i] and pos[i] < cm[i] + h)) inside = false;
                    }
                    const slots = leafActive(leaf);
                    const active = slots orelse true;
                    const by_slots = slots != null;
                    if (dump) |lines| {
                        if (lines.items.len < 16) {
                            var raw: [0x28 * 2]u8 = undefined;
                            const bytes: [*]const u8 = @ptrFromInt(leaf);
                            for (0..0x28) |i| _ = std.fmt.bufPrint(raw[i * 2 ..][0..2], "{x:0>2}", .{bytes[i]}) catch {};
                            if (std.fmt.allocPrint(gpa, "leaf=0x{x} cube_min={any} voxel={any} inside={} active={} rule={s} raw={s}", .{
                                leaf, cm, pos, inside, active, if (by_slots) "slot_flag" else "presence", raw,
                            })) |line| {
                                lines.append(gpa, line) catch gpa.free(line);
                            } else |_| {}
                        }
                    }
                    if (!inside) {
                        scan.rejected += 1;
                        continue;
                    }
                    scan.leaves += 1;
                    if (by_slots) scan.by_slots += 1 else scan.by_presence += 1;
                    out.append(gpa, .{ .pos = pos, .active = active, .by_slots = by_slots }) catch return scan;
                }
            }
        }
    }
    return scan;
}

const FireScanCtx = struct {
    sim: *Sim,
    field: *FireField,
    budget: *u32,
    voxels: *std.ArrayList(FireVoxel),
    raw_bodies: *u32,
    now_ms: u64,

    fn visit(self: *const FireScanCtx, _: u32, body: usize) bool {
        const matrix = bodyWorldMatrix(body) orelse return true;
        if (!matrixOk(&matrix)) return true;
        self.voxels.clearRetainingCapacity();
        if ((readableAt(body + OFF_BODY_FIRE_COUNT, 4) and readU32At(body + OFF_BODY_FIRE_COUNT) != 0) or
            (readableAt(body + OFF_BODY_DEFAULT_FIRE, 4) and readU32At(body + OFF_BODY_DEFAULT_FIRE) != 0))
        {
            self.raw_bodies.* += 1;
        }
        var dump_lines: DumpLines = .empty;
        defer freeDumpLines(&dump_lines);
        const want_dump = verbose and !self.sim.fire_dumped.contains(body) and
            readableAt(body + OFF_BODY_FIRE_COUNT, 4) and readableAt(body + OFF_BODY_DEFAULT_FIRE, 4) and
            (readU32At(body + OFF_BODY_FIRE_COUNT) != 0 or readU32At(body + OFF_BODY_DEFAULT_FIRE) != 0);
        const scan = collectFireVoxels(body, self.budget, self.voxels, if (want_dump) &dump_lines else null);
        // Plausible range: the body's known cell box plus a margin.
        const known: ?[2][3]i32 = if (self.sim.staging.getPtr(body)) |g| .{
            g.min,
            .{ g.min[0] +% @as(i32, @intCast(g.dims[0])), g.min[1] +% @as(i32, @intCast(g.dims[1])), g.min[2] +% @as(i32, @intCast(g.dims[2])) },
        } else null;
        var kept: u32 = 0;
        var lo = [3]i32{ std.math.maxInt(i32), std.math.maxInt(i32), std.math.maxInt(i32) };
        var hi = [3]i32{ std.math.minInt(i32), std.math.minInt(i32), std.math.minInt(i32) };
        outer: for (self.voxels.items) |v| {
            if (!v.active) continue;
            if (known) |k| {
                for (0..3) |i| {
                    if (v.pos[i] < k[0][i] -| FIRE_BOX_MARGIN or v.pos[i] > k[1][i] +| FIRE_BOX_MARGIN) continue :outer;
                }
            }
            for (0..3) |i| {
                lo[i] = @min(lo[i], v.pos[i]);
                hi[i] = @max(hi[i], v.pos[i]);
            }
            kept += 1;
            self.field.insert(voxelToWorld(&matrix, v.pos[0], v.pos[1], v.pos[2]), FIRE_TEMP_C, null);
        }
        if (verbose) {
            if (dump_lines.items.len > 0) {
                self.sim.fire_dumped.put(gpa, body, {}) catch {};
                logf("blocktemp: fire dump body=0x{x} records={d} cells_field={d} leaves={d} inner={d} rejected={d} active_rule: slot_flag={d} presence_fallback={d} (leaf = int3 voxel at +0x18 inside the path cube; slot_flag = any slot active byte set, presence = slot vector unreadable)", .{
                    body, scan.records, scan.cells_field, scan.leaves, scan.inner, scan.rejected, scan.by_slots, scan.by_presence,
                });
                for (dump_lines.items) |line| logf("blocktemp: fire dump {s}", .{line});
            }
            if (kept > 0) {
                const last = self.sim.fire_log_ms.get(body) orelse 0;
                if (self.now_ms -| last >= FIRE_LOG_INTERVAL_MS) {
                    self.sim.fire_log_ms.put(gpa, body, self.now_ms) catch {};
                    logf("blocktemp: fire body=0x{x} burning_voxels={d} of {d} leaves (cells_field={d}) bbox={any}..{any}", .{
                        body, kept, scan.leaves, scan.cells_field, lo, hi,
                    });
                }
            }
        }
        return true;
    }
};

const Begun = struct { dt: f32, phase: u32 };

const SimMethods = struct {
    /// Re-read every body's fire octrees into the world-space `FireField`.
    /// Bodies keep their last field only until the next scan replaces it.
    fn scanFires(self: *Sim, scene: usize, now_ms: u64) void {
        if (now_ms -| self.last_fire_scan_ms < FIRE_SCAN_INTERVAL_MS) return;
        self.last_fire_scan_ms = now_ms;
        fault.set(stage.BLOCKTEMP_ENUMERATE);
        var field: FireField = .{};
        var budget: u32 = MAX_FIRE_NODES_PER_SCAN;
        var voxels: std.ArrayList(FireVoxel) = .empty;
        defer voxels.deinit(gpa);
        var raw_bodies: u32 = 0;
        const ctx: FireScanCtx = .{ .sim = self, .field = &field, .budget = &budget, .voxels = &voxels, .raw_bodies = &raw_bodies, .now_ms = now_ms };
        _ = forEachBody(scene, &ctx, FireScanCtx.visit);
        self.fire_bodies_raw = raw_bodies;
        // Hot engine/jet/radiator cells radiate to every body (rebuilt each scan).
        {
            var drop: std.ArrayList(usize) = .empty;
            defer drop.deinit(gpa);
            var keys = self.heat_src.keyIterator();
            while (keys.next()) |body| {
                if (!self.staging.contains(body.*)) drop.append(gpa, body.*) catch {};
            }
            for (drop.items) |body| {
                if (self.heat_src.fetchRemove(body)) |kv| {
                    var list = kv.value;
                    list.deinit(gpa);
                }
            }
        }
        var it = self.heat_src.valueIterator();
        while (it.next()) |points| {
            for (points.items) |p| field.insert(p.pos, p.temp_c, p.owner);
        }
        // Exhaust outlets: the plume emitter sits at the pipe end, so the last
        // blocks of an exhaust run heat up (capped: the gas is hotter than the
        // pipe wall it touches).
        {
            const keys = state.EXHAUST_EMITTERS.lock();
            defer state.EXHAUST_EMITTERS.unlock();
            var sources = self.sources.iterator();
            while (sources.next()) |entry| {
                if (std.mem.indexOfScalar(usize, keys.items, entry.key_ptr.*) != null) {
                    field.insert(entry.value_ptr.pos, util.fmin(entry.value_ptr.temp_c, EXHAUST_PIPE_C), null);
                }
            }
        }
        self.fire.deinit();
        self.fire = field;
    }

    /// Seconds this tick represents (scaled by the interlace factor) and the
    /// phase whose bodies are due.
    fn beginTick(self: *Sim, now_us: u64) Begun {
        const raw: f32 = if (self.last_tick_us) |previous| @floatCast(util.f(now_us -| previous) / 1.0e6) else DEFAULT_TICK_DT_S;
        self.last_tick_us = now_us;
        const dt = if (std.math.isFinite(raw) and raw > 0.0) @min(raw, MAX_TICK_DT_S) else DEFAULT_TICK_DT_S;
        const phase = self.tick_phase;
        self.tick_phase = (self.tick_phase + 1) % SIM_INTERLACE;
        self.tick_cells = 0;
        // Grids persist across ticks (allocations are reused), so only what is
        // left after the staging map's current size is available to new bodies.
        var held: usize = 0;
        var it = self.staging.valueIterator();
        while (it.next()) |grid| held += grid.temps_c.items.len * @sizeOf(f32);
        self.grid_budget = MAX_GRID_BYTES -| held;
        return .{ .dt = dt * util.f32_(SIM_INTERLACE), .phase = phase };
    }
};

const BoundSource = struct { temp_c: f32, cells: u32, voxel: [3]i32, world: Vec3, source_pos: Vec3 };
const Conducted = struct { pos: Vec3, temp_c: f32 };

/// Advance one body: re-enumerate it on its interlace turn, keep its
/// published grid alive otherwise.
fn processBodyFn(self: *Sim, body_ptr: usize, vehicle_id: u32, now_ms: u64, dt: f32, sun_factor: f32, tick_phase: u32) void {
    fault.set(stage.BLOCKTEMP_COLLECT_CELLS);
    const body_id = readU32At(body_ptr + OFF_BODY_ID);
    const body_key: BodyKey = .{ .vehicle_id = vehicle_id, .body_id = body_id };

    // Interlace phase is assigned once per body identity and stays stable.
    const meta_entry = self.bodies.getOrPut(gpa, body_key) catch return;
    if (!meta_entry.found_existing) {
        meta_entry.value_ptr.* = .{
            .last_seen_ms = now_ms,
            .phase = self.next_phase % SIM_INTERLACE,
            .air_c = std.math.nan(f32),
            .ground_air_c = std.math.nan(f32),
            .last_hot_log_ms = 0,
        };
        self.next_phase +%= 1;
    }
    const phase = meta_entry.value_ptr.phase;
    meta_entry.value_ptr.last_seen_ms = now_ms;

    // Only a body's interlace turn re-enumerates and steps it; every other
    // tick just keeps its last published grid alive. A body without a grid
    // yet is always built so it appears on its first sighting.
    if (phase != tick_phase and self.staging.contains(body_ptr)) {
        self.seen.put(gpa, body_ptr, {}) catch {};
        return;
    }

    const matrix = bodyWorldMatrix(body_ptr) orelse {
        logOnce(LOG_MATRIX, "blocktemp: body 0x{x} has no physics object, skipped", .{body_ptr});
        return;
    };
    if (!matrixOk(&matrix)) {
        logOnce(LOG_MATRIX, "blocktemp: body 0x{x} has an invalid matrix, skipped", .{body_ptr});
        return;
    }

    // One ambient probe per body update, not per cell.
    const origin: Vec3 = .{ matrix[12], matrix[13], matrix[14] };

    self.pending.clearRetainingCapacity();
    const collected = collectCells(body_ptr, &self.pending);
    if (!collected.ok or self.pending.items.len == 0) return;

    const bounds = gridBounds(self.pending.items) orelse {
        logOnce(LOG_SKIP, "blocktemp: body 0x{x} exceeds grid caps (cells={d}), skipped", .{ body_ptr, self.pending.items.len });
        return;
    };
    const min = bounds.min;
    const dims = bounds.dims;
    const total = bounds.total;

    // Grid memory budget: a giant spawn (or a garbage count) must not turn
    // into gigabytes of f32 grids. Bodies beyond the budget are skipped
    // (and so dropped from the published map) rather than truncated.
    const grid_bytes = total * @sizeOf(f32);
    self.tick_cells += total;
    const existing: usize = if (self.staging.getPtr(body_ptr)) |g| g.temps_c.items.len * @sizeOf(f32) else 0;
    const growth = grid_bytes -| existing;
    if (growth > self.grid_budget) {
        logOnce(LOG_BUDGET, "blocktemp: grid budget ({d} MB) exhausted; body 0x{x} with {d} cells skipped", .{ MAX_GRID_BYTES >> 20, body_ptr, total });
        return;
    }
    self.grid_budget -= growth;

    if (verbose) {
        const first = self.pending.items[0];
        logOnce(LOG_CELLS, "blocktemp: body 0x{x} vid={d} bid={d} components={d} cells={d} min={any} dims={any} first_cell=({d},{d},{d}) d0={d:.0} d1={d:.0} d2={d:.0} t=({d:.1},{d:.1},{d:.1})", .{
            body_ptr, vehicle_id, body_id, collected.components, self.pending.items.len, min, dims, first.x, first.y, first.z,
            matrix[0], matrix[5], matrix[10], matrix[12], matrix[13], matrix[14],
        });
    }

    const staged = self.staging.getOrPut(gpa, body_ptr) catch return;
    if (!staged.found_existing) staged.value_ptr.* = .{ .min = min, .dims = dims };
    const grid = staged.value_ptr;
    grid.min = min;
    grid.dims = dims;
    if (grid.temps_c.items.len != total) {
        grid.temps_c.clearRetainingCapacity();
        grid.temps_c.appendNTimes(gpa, std.math.nan(f32), total) catch {
            grid.temps_c.clearRetainingCapacity();
            return;
        };
    } else {
        @memset(grid.temps_c.items, std.math.nan(f32));
    }

    const t_air = airTemperatureAt(origin);
    const t_ground = airTemperatureAt(.{ origin[0], 0.0, origin[2] });
    if (self.bodies.getPtr(body_key)) |meta| {
        meta.air_c = t_air;
        meta.ground_air_c = t_ground;
    }

    self.slots.clearRetainingCapacity();
    var hot_points: std.ArrayList(HeatPoint) = .empty;
    var changed = false;
    const dims_usize = [3]usize{ dims[0], dims[1], dims[2] };

    var new_bound: fx.HashMap(usize, BoundSource) = .empty;
    defer new_bound.deinit(gpa);
    var conducted: fx.HashMap([3]i32, Conducted) = .empty;
    defer conducted.deinit(gpa);
    for (self.pending.items) |raw| {
        var source_c: ?f32 = null;
        if (self.sources.get(raw.component)) |src| {
            const w = voxelToWorld(&matrix, raw.x, raw.y, raw.z);
            const d = @sqrt(util.sq(w[0] - src.pos[0]) + util.sq(w[1] - src.pos[1]) + util.sq(w[2] - src.pos[2]));
            if (d <= SOURCE_MAX_DIST_M) {
                source_c = src.temp_c;
                // A hot source cell radiates to nearby cells of every body
                // (hood over an engine), via the heat-point field.
                if (src.temp_c > t_air + 5.0) {
                    hot_points.append(gpa, .{ .pos = w, .temp_c = src.temp_c, .owner = .{ .body = body_ptr, .cell = .{ raw.x, raw.y, raw.z } } }) catch {};
                }
                if (!self.bound.contains(raw.component)) {
                    if (new_bound.getOrPut(gpa, raw.component) catch null) |e| {
                        if (!e.found_existing) e.value_ptr.* = .{ .temp_c = src.temp_c, .cells = 0, .voxel = .{ raw.x, raw.y, raw.z }, .world = w, .source_pos = src.pos };
                        e.value_ptr.cells += 1;
                    }
                }
            } else if (!self.bound.contains(raw.component)) {
                logOnce(LOG_SRC_FAR, "blocktemp: source component=0x{x} temp={d:.1} C rejected: {d:.1} m from its voxel ({d},{d},{d}) world=({d:.1},{d:.1},{d:.1}) source_pos=({d:.1},{d:.1},{d:.1})", .{
                    raw.component, src.temp_c, d, raw.x, raw.y, raw.z, w[0], w[1], w[2], src.pos[0], src.pos[1], src.pos[2],
                });
            }
        }
        const local = [3]usize{ @intCast(raw.x - min[0]), @intCast(raw.y - min[1]), @intCast(raw.z - min[2]) };
        const index = gridIndex(local, dims_usize);
        if (index >= total) continue;
        const key: CellKey = .{ .vehicle_id = vehicle_id, .body_id = body_id, .x = raw.x, .y = raw.y, .z = raw.z };
        const capacity = util.fmax(raw.mass_kg * SPECIFIC_HEAT_J_KG_K, MIN_CAPACITY_J_K);
        const cell = self.cells.getOrPut(gpa, key) catch continue;
        const temperature = if (cell.found_existing) blk: {
            cell.value_ptr.capacity_j_k = capacity;
            cell.value_ptr.last_seen_ms = now_ms;
            break :blk cell.value_ptr.temp_c;
        } else blk: {
            cell.value_ptr.* = .{ .temp_c = t_air, .capacity_j_k = capacity, .t_env_c = t_air, .last_seen_ms = now_ms };
            changed = true;
            break :blk t_air;
        };
        grid.temps_c.items[index] = temperature;
        self.slots.append(gpa, .{
            .index = index,
            .x = raw.x,
            .y = raw.y,
            .z = raw.z,
            .capacity_j_k = capacity,
            .key = key,
            .source_c = source_c,
            .component = raw.component,
        }) catch {};
    }
    if (self.heat_src.fetchRemove(body_ptr)) |old| {
        var list = old.value;
        list.deinit(gpa);
    }
    if (hot_points.items.len == 0) {
        hot_points.deinit(gpa);
    } else {
        self.heat_src.put(gpa, body_ptr, hot_points) catch hot_points.deinit(gpa);
    }
    {
        var plumes: std.ArrayList(Plume) = .empty;
        defer plumes.deinit(gpa);
        jetPlumes(self.pending.items, t_air, &plumes);
        flir.setBodyPlumes(body_ptr, plumes.items);
    }
    var bound_it = new_bound.iterator();
    while (bound_it.next()) |entry| {
        self.bound.put(gpa, entry.key_ptr.*, {}) catch {};
        const b = entry.value_ptr;
        logging.vlogf("blocktemp: source bound component=0x{x} temp={d:.1} C cells={d} voxel={any} world=({d:.1},{d:.1},{d:.1}) source_pos=({d:.1},{d:.1},{d:.1})", .{
            entry.key_ptr.*, b.temp_c, b.cells, b.voxel, b.world[0], b.world[1], b.world[2], b.source_pos[0], b.source_pos[1], b.source_pos[2],
        });
    }

    fault.set(stage.BLOCKTEMP_PROBE);
    if (self.slots.items.len > 0) {
        self.scratch.clearRetainingCapacity();
        self.scratch.appendSlice(gpa, grid.temps_c.items) catch return;
        // Per-body reject: a body nowhere near any heat point skips the
        // 27-bucket lookup for every one of its cells.
        const near_heat = self.fire.len > 0 and blk: {
            var lo = Vec3{ std.math.floatMax(f64), std.math.floatMax(f64), std.math.floatMax(f64) };
            var hi = Vec3{ -std.math.floatMax(f64), -std.math.floatMax(f64), -std.math.floatMax(f64) };
            for (0..8) |c| {
                const v = [3]i32{
                    if (c & 1 != 0) min[0] + @as(i32, @intCast(dims[0])) - 1 else min[0],
                    if (c & 2 != 0) min[1] + @as(i32, @intCast(dims[1])) - 1 else min[1],
                    if (c & 4 != 0) min[2] + @as(i32, @intCast(dims[2])) - 1 else min[2],
                };
                const w = voxelToWorld(&matrix, v[0], v[1], v[2]);
                for (0..3) |i| {
                    lo[i] = util.fmin(lo[i], w[i]);
                    hi[i] = util.fmax(hi[i], w[i]);
                }
            }
            // Half a voxel of slack for cell extents.
            const e: f64 = CELL_M;
            break :blk self.fire.nearBox(.{ lo[0] - e, lo[1] - e, lo[2] - e }, .{ hi[0] + e, hi[1] + e, hi[2] + e });
        };
        const h_env_w_k = H_ENV_W_M2_K * FACE_AREA_M2;
        const h_air_w_k = H_AIR_W_M2_K * FACE_AREA_M2;
        const solar_w = SOLAR_FLUX_W_M2 * FACE_AREA_M2 * sun_factor;
        var seekers: std.ArrayList(usize) = .empty;
        defer seekers.deinit(gpa);
        {
            const all = SEEKERS.lock();
            defer SEEKERS.unlock();
            for (all.items) |e| seekers.append(gpa, e.component) catch {};
        }
        for (self.slots.items) |slot| {
            if (slot.index >= self.scratch.items.len) continue;
            const t = self.scratch.items[slot.index];
            if (!std.math.isFinite(t)) continue;
            const local = [3]i32{ slot.x - min[0], slot.y - min[1], slot.z - min[2] };
            var neighbour_count: u32 = 0;
            var neighbour_sum: f32 = 0.0;
            for (NEIGHBOURS) |offset| {
                const nx = local[0] + offset[0];
                const ny = local[1] + offset[1];
                const nz = local[2] + offset[2];
                if (nx < 0 or ny < 0 or nz < 0 or nx >= dims[0] or ny >= dims[1] or nz >= dims[2]) continue;
                const neighbour_index = gridIndex(.{ @intCast(nx), @intCast(ny), @intCast(nz) }, dims_usize);
                if (neighbour_index < self.scratch.items.len) {
                    const neighbour = self.scratch.items[neighbour_index];
                    if (std.math.isFinite(neighbour)) {
                        neighbour_count += 1;
                        neighbour_sum += neighbour;
                    }
                }
            }

            const t_env = if (!near_heat)
                t_air
            else
                self.fire.envTemp(voxelToWorld(&matrix, slot.x, slot.y, slot.z), t_air, .{ .body = body_ptr, .cell = .{ slot.x, slot.y, slot.z } });
            if (self.cells.getPtr(slot.key)) |cell| cell.t_env_c = t_env;
            const exposed = util.f32_(6 -| neighbour_count);
            const input: StepInputs = .{
                .capacity_j_k = slot.capacity_j_k,
                .dt_s = dt,
                .t_env_c = t_env,
                .t_air_c = t_air,
                // Only exposed faces see the fire/heat field, and only when a heat point is
                // near (t_env > t_air); otherwise plain air convection (h_air) applies, so a
                // climbing jet does not dump its heat to cold air at fire-coupling rates.
                .h_env_w_k = if (t_env > t_air) h_env_w_k * exposed else 0.0,
                .h_air_w_k = h_air_w_k,
                .k_cond_w_k = K_COND_W_PER_K,
                .neighbour_count = neighbour_count,
                .neighbour_sum_c = neighbour_sum,
                // Only exposed faces take sun, about half of them facing it;
                // a fully buried cell (6 neighbours) gets none.
                .solar_w = solar_w * 0.5 * exposed / 6.0,
                .source_c = slot.source_c,
            };
            const next = stepTemperature(t, &input);
            if (@abs(next - t) > TEMP_CHANGE_EPS_C) changed = true;
            grid.temps_c.items[slot.index] = next;
            if (self.cells.getPtr(slot.key)) |cell| cell.temp_c = next;
            if (seekers.items.len > 0 and std.mem.indexOfScalar(usize, seekers.items, slot.component) != null) {
                noteSeekerVehicle(slot.component, body_key.vehicle_id);
            }
            // Conduction-heated cells (a wing next to a jet engine) warm
            // parts of OTHER bodies touching them (landing gear on hinges).
            // Only surface cells can touch another body; 0.5 m cells
            // keep the field small (a hot wing has thousands of cells).
            if (slot.source_c == null and neighbour_count < 6 and next > t_air + CONDUCTED_POINT_EXCESS_C) {
                const key = [3]i32{ @divFloor(slot.x, 2), @divFloor(slot.y, 2), @divFloor(slot.z, 2) };
                const full = conducted.count() >= MAX_CONDUCTED_POINTS;
                if (conducted.getPtr(key)) |e| {
                    if (next > e.temp_c) e.* = .{ .pos = voxelToWorld(&matrix, slot.x, slot.y, slot.z), .temp_c = next };
                } else if (!full) {
                    conducted.put(gpa, key, .{ .pos = voxelToWorld(&matrix, slot.x, slot.y, slot.z), .temp_c = next }) catch {};
                }
            }
        }
        var half: [3]f64 = undefined;
        var mid: [3]i32 = undefined;
        for (0..3) |i| {
            half[i] = 0.5 * @as(f64, CELL_M) * util.f(dims[i]);
            mid[i] = min[i] + @as(i32, @intCast(dims[i] / 2));
        }
        publishBodyIr(body_ptr, .{
            .stamp_ms = now_ms,
            .stamp_tick = SCENE_TICKS.load(.monotonic),
            .vehicle_id = body_key.vehicle_id,
            .center = voxelToWorld(&matrix, mid[0], mid[1], mid[2]),
            .vel = .{ 0, 0, 0 },
            .sphere_r = @sqrt(util.sq(half[0]) + util.sq(half[1]) + util.sq(half[2])),
        });
    }
    if (conducted.count() > 0) {
        if (self.heat_src.getOrPut(gpa, body_ptr) catch null) |entry| {
            if (!entry.found_existing) entry.value_ptr.* = .empty;
            var it = conducted.valueIterator();
            while (it.next()) |c| {
                entry.value_ptr.append(gpa, .{ .pos = c.pos, .temp_c = c.temp_c, .owner = .{ .body = body_ptr, .cell = .{ BODY_WIDE_OWNER, 0, 0 } } }) catch break;
            }
        }
    }

    if (verbose) self.logHottest(body_key, body_ptr, &matrix, now_ms);

    // Monotonic per-body version: the renderer's upload trigger. Only a
    // changed grid gets a new version, so an unchanged body is not uploaded
    // again every time it is stepped.
    if (changed) {
        const version = self.next_version;
        self.next_version +%= 1;
        if (self.staging.getPtr(body_ptr)) |g| g.version = version;
    }

    self.seen.put(gpa, body_ptr, {}) catch {};
    self.stepped.put(gpa, body_ptr, {}) catch {};
}

/// Top-3 hottest cells of a body, at most every `HOT_LOG_INTERVAL_MS`.
fn logHottestFn(self: *Sim, key: BodyKey, body_ptr: usize, matrix: *const [16]f64, now_ms: u64) void {
    const meta = self.bodies.getPtr(key) orelse return;
    if (now_ms -| meta.last_hot_log_ms < HOT_LOG_INTERVAL_MS) return;
    meta.last_hot_log_ms = now_ms;
    const Hot = struct { t: f32, slot: *const CellSlot };
    var top: std.ArrayList(Hot) = .empty;
    defer top.deinit(gpa);
    for (self.slots.items) |*s| {
        const c = self.cells.get(s.key) orelse continue;
        if (std.math.isFinite(c.temp_c)) top.append(gpa, .{ .t = c.temp_c, .slot = s }) catch {};
    }
    std.mem.sort(Hot, top.items, {}, struct {
        fn lessThan(_: void, a: Hot, b: Hot) bool {
            return a.t > b.t;
        }
    }.lessThan);
    var line: std.ArrayList(u8) = .empty;
    defer line.deinit(gpa);
    line.print(gpa, "blocktemp: hottest body=0x{x} vid={d} bid={d} cells={d}:", .{ body_ptr, key.vehicle_id, key.body_id, self.slots.items.len }) catch return;
    for (top.items[0..@min(top.items.len, 3)]) |h| {
        const s = h.slot;
        const w = voxelToWorld(matrix, s.x, s.y, s.z);
        const env = if (self.cells.get(s.key)) |c| c.t_env_c else std.math.nan(f32);
        var src_buf: [32]u8 = undefined;
        const src = if (s.source_c) |v| std.fmt.bufPrint(&src_buf, "{d:.1}", .{v}) catch "?" else "none";
        line.print(gpa, " [voxel=({d},{d},{d}) world=({d:.1},{d:.1},{d:.1}) t={d:.1} C t_env={d:.1} src={s} comp=0x{x}]", .{
            s.x, s.y, s.z, w[0], w[1], w[2], h.t, env, src, s.component,
        }) catch {};
    }
    logging.logLine(line.items);
}

/// Remove every entry of a body-pointer-keyed grid map not in `seen`,
/// releasing its grid.
fn dropUnseen(map: *fx.HashMap(usize, BodyGrid), seen: *const fx.HashSet(usize)) void {
    var drop: std.ArrayList(usize) = .empty;
    defer drop.deinit(gpa);
    var keys = map.keyIterator();
    while (keys.next()) |ptr| {
        if (!seen.contains(ptr.*)) drop.append(gpa, ptr.*) catch {};
    }
    for (drop.items) |ptr| {
        if (map.fetchRemove(ptr)) |kv| {
            var grid = kv.value;
            grid.deinit();
        }
    }
}

/// Copy this tick's stepped grids into `PUBLISHED` and drop the bodies
/// that are gone.
///
/// A body that is not due is not stepped, so it is not copied either: the
/// published map keeps exactly the grid it last published (swapping whole
/// maps instead published the stale half of the alternating staging grid
/// and made the image flicker).
fn publishFn(self: *Sim, now_ms: u64) void {
    {
        const published = PUBLISHED.lock();
        defer PUBLISHED.unlock();
        dropUnseen(published, &self.seen);
        var it = self.stepped.keyIterator();
        while (it.next()) |ptr| {
            const grid = self.staging.getPtr(ptr.*) orelse continue;
            // Reuse the published grid's allocation: the render thread
            // only ever sees complete grids either way.
            const slot = published.getOrPut(gpa, ptr.*) catch continue;
            if (!slot.found_existing) slot.value_ptr.* = .{};
            if (!slot.value_ptr.copyFrom(grid)) {
                var failed = slot.value_ptr.*;
                failed.deinit();
                _ = published.remove(ptr.*);
            }
        }
    }
    // Staging keeps the body's grid for the next interlaced update; bodies
    // that are gone release their memory here.
    dropUnseen(&self.staging, &self.seen);

    if (now_ms -| self.last_prune_ms >= PRUNE_INTERVAL_MS) {
        self.last_prune_ms = now_ms;
        {
            var drop: std.ArrayList(CellKey) = .empty;
            defer drop.deinit(gpa);
            var it = self.cells.iterator();
            while (it.next()) |e| {
                if (now_ms -| e.value_ptr.last_seen_ms >= STALE_BODY_MS) drop.append(gpa, e.key_ptr.*) catch {};
            }
            for (drop.items) |k| _ = self.cells.remove(k);
        }
        {
            var drop: std.ArrayList(BodyKey) = .empty;
            defer drop.deinit(gpa);
            var it = self.bodies.iterator();
            while (it.next()) |e| {
                if (now_ms -| e.value_ptr.last_seen_ms >= STALE_BODY_MS) drop.append(gpa, e.key_ptr.*) catch {};
            }
            for (drop.items) |k| _ = self.bodies.remove(k);
        }
    }
}

// ---------------------------------------------------------------------------
// Self-checks (pure step only; the game-facing walk cannot run off-target)
// ---------------------------------------------------------------------------

const testing = std.testing;

fn isolated(capacity_j_k: f32, dt_s: f32, t_env_c: f32, t_air_c: f32, solar_w: f32) StepInputs {
    return .{
        .capacity_j_k = capacity_j_k,
        .dt_s = dt_s,
        .t_env_c = t_env_c,
        .t_air_c = t_air_c,
        .h_env_w_k = H_ENV_W_M2_K * FACE_AREA_M2,
        .h_air_w_k = H_AIR_W_M2_K * FACE_AREA_M2,
        .k_cond_w_k = K_COND_W_PER_K,
        .neighbour_count = 0,
        .neighbour_sum_c = 0.0,
        .solar_w = solar_w,
        .source_c = null,
    };
}

fn conductionOnly(capacity_j_k: f32, dt_s: f32, neighbour: f32) StepInputs {
    return .{
        .capacity_j_k = capacity_j_k,
        .dt_s = dt_s,
        .t_env_c = 0.0,
        .t_air_c = 0.0,
        .h_env_w_k = 0.0,
        .h_air_w_k = 0.0,
        .k_cond_w_k = K_COND_W_PER_K,
        .neighbour_count = 1,
        .neighbour_sum_c = neighbour,
        .solar_w = 0.0,
        .source_c = null,
    };
}

test "conduction conserves energy without environment" {
    const capacity: f32 = 500.0; // same on both cells -> symmetric pair
    const dt: f32 = 0.5;
    var a: f32 = 100.0;
    var b: f32 = 0.0;
    const initial_energy = capacity * (a + b);
    // 5000 steps of 0.5 s is ~15 difference time constants (C/(2k) = 167 s),
    // so the pair should be fully equalised at the end.
    for (0..5000) |_| {
        // Both cells see the OTHER cell's old temperature: the pair's
        // energy change cancels exactly, so conduction only redistributes.
        const next_a = stepTemperature(a, &conductionOnly(capacity, dt, b));
        const next_b = stepTemperature(b, &conductionOnly(capacity, dt, a));
        a = next_a;
        b = next_b;
    }
    const final_energy = capacity * (a + b);
    try testing.expect(@abs(final_energy - initial_energy) < 0.1);
    try testing.expect(a < 100.0 and b > 0.0);
    try testing.expect(@abs(a - b) < 1.0);
}

test "heavier cell heats slower" {
    const light = isolated(500.0, 1.0 / 60.0, 200.0, 20.0, 0.0);
    const heavy = isolated(5000.0, 1.0 / 60.0, 200.0, 20.0, 0.0);
    const t0: f32 = 20.0;
    const light_rise = stepTemperature(t0, &light) - t0;
    const heavy_rise = stepTemperature(t0, &heavy) - t0;
    try testing.expect(light_rise > 0.0 and heavy_rise > 0.0);
    try testing.expect(heavy_rise < light_rise);
    try testing.expect(@abs(light_rise / heavy_rise - 10.0) < 0.1);
}

test "step is stable at extreme dt and capacity" {
    const input: StepInputs = .{
        .capacity_j_k = 1.0,
        .dt_s = 1000.0,
        .t_env_c = 900.0,
        .t_air_c = 20.0,
        .h_env_w_k = H_ENV_W_M2_K * FACE_AREA_M2,
        .h_air_w_k = H_AIR_W_M2_K * FACE_AREA_M2,
        .k_cond_w_k = K_COND_W_PER_K,
        .neighbour_count = 6,
        .neighbour_sum_c = 6.0 * 20.0,
        .solar_w = 25.0,
        .source_c = null,
    };
    const next = stepTemperature(20.0, &input);
    try testing.expect(std.math.isFinite(next));
    try testing.expect(next >= 20.0 and next <= 900.0);
}

test "non-finite state recovers to air" {
    const input = isolated(500.0, 1.0 / 60.0, 100.0, 15.0, 0.0);
    try testing.expect(@abs(stepTemperature(std.math.nan(f32), &input) - 15.0) < std.math.floatEps(f32));
    var bad_capacity = input;
    bad_capacity.capacity_j_k = std.math.nan(f32);
    try testing.expect(@abs(stepTemperature(20.0, &bad_capacity) - 15.0) < std.math.floatEps(f32));
}

test "source cell reaches its temp and warms neighbour" {
    const capacity: f32 = 5000.0;
    const dt: f32 = 1.0 / 15.0;
    var hot: f32 = 20.0;
    var next_door: f32 = 20.0;
    for (0..15 * 10) |_| {
        var a = isolated(capacity, dt, 20.0, 20.0, 0.0);
        a.neighbour_count = 1;
        a.neighbour_sum_c = next_door;
        a.source_c = 70.0;
        var b = isolated(capacity, dt, 20.0, 20.0, 0.0);
        b.neighbour_count = 1;
        b.neighbour_sum_c = hot;
        const na = stepTemperature(hot, &a);
        const nb = stepTemperature(next_door, &b);
        hot = na;
        next_door = nb;
    }
    try testing.expect(@abs(hot - 70.0) < 2.0);
    try testing.expect(next_door > 20.05 and next_door < hot);
}

test "bounds reject oversized and empty grids" {
    try testing.expect(gridBounds(&.{}) == null);
    const one = [_]RawCell{.{ .x = 0, .y = 0, .z = 0, .mass_kg = 1.0, .component = 0 }};
    const b = gridBounds(&one).?;
    try testing.expectEqual([3]i32{ 0, 0, 0 }, b.min);
    try testing.expectEqual([3]u32{ 1, 1, 1 }, b.dims);
    try testing.expectEqual(@as(usize, 1), b.total);
    const huge = [_]RawCell{
        .{ .x = 0, .y = 0, .z = 0, .mass_kg = 1.0, .component = 0 },
        .{ .x = MAX_GRID_DIM, .y = 0, .z = 0, .mass_kg = 1.0, .component = 0 },
    };
    try testing.expect(gridBounds(&huge) == null);
}

test "publish keeps the last stepped grid for a body that is not due" {
    // Body 0x1000 is stepped with 40 C, then a tick arrives on which it is
    // not due (only `seen`). The staging grid still holds the previous
    // build, but publishing it would flip the image back and forth.
    const body: usize = 0x1000;
    var sim: Sim = .{};
    var grid: BodyGrid = .{ .min = .{ 0, 0, 0 }, .dims = .{ 1, 1, 1 }, .version = 7 };
    try grid.temps_c.append(gpa, 40.0);
    try sim.staging.put(gpa, body, grid);
    try sim.seen.put(gpa, body, {});
    try sim.stepped.put(gpa, body, {});
    sim.publish(0);

    sim.staging.getPtr(body).?.temps_c.items[0] = 99.0;
    sim.stepped.clearRetainingCapacity();
    sim.publish(0);
    {
        const published = publishedLock();
        defer publishedUnlock();
        const entry = published.get(body).?;
        try testing.expectEqualSlices(f32, &.{40.0}, entry.temps_c.items);
        try testing.expectEqual(@as(u64, 7), entry.version);
    }
    // A body that is gone is dropped from both maps.
    sim.seen.clearRetainingCapacity();
    sim.publish(0);
    {
        const published = publishedLock();
        defer publishedUnlock();
        try testing.expect(published.get(body) == null);
    }
    try testing.expect(sim.staging.get(body) == null);
}

test "grid index is x fastest matching texture layout" {
    const dims = [3]usize{ 3, 4, 5 };
    try testing.expectEqual(@as(usize, 0), gridIndex(.{ 0, 0, 0 }, dims));
    try testing.expectEqual(@as(usize, 1), gridIndex(.{ 1, 0, 0 }, dims));
    try testing.expectEqual(@as(usize, 3), gridIndex(.{ 0, 1, 0 }, dims));
    try testing.expectEqual(@as(usize, 12), gridIndex(.{ 0, 0, 1 }, dims));
    try testing.expectEqual(@as(usize, 59), gridIndex(.{ 2, 3, 4 }, dims));
}

test "fire falloff and hash lookup" {
    // Falloff: full inside 0.4 m, linear to air at 1.0 m.
    try testing.expectEqual(FIRE_TEMP_C, fireTempAt(0.0, 20.0));
    try testing.expectEqual(FIRE_TEMP_C, fireTempAt(0.4, 20.0));
    try testing.expect(@abs(fireTempAt(0.7, 20.0) - 210.0) < 0.5);
    try testing.expectEqual(@as(f32, 20.0), fireTempAt(1.0, 20.0));
    try testing.expectEqual(@as(f32, 20.0), fireTempAt(5.0, 20.0));
    // Never cools below a hotter air temperature.
    try testing.expectEqual(@as(f32, 500.0), fireTempAt(2.0, 500.0));

    var field: FireField = .{};
    defer field.deinit();
    try testing.expectEqual(@as(f32, 20.0), field.envTemp(.{ 0, 0, 0 }, 20.0, null));
    // Points straddling a bucket border and in a negative bucket.
    field.insert(.{ 0.99, 0.0, -0.01 }, FIRE_TEMP_C, null);
    field.insert(.{ -10.2, 3.0, 7.7 }, FIRE_TEMP_C, null);
    try testing.expectEqual(@as(usize, 2), field.len);
    // Neighbour bucket (x = 1.2) is within 0.4 m of the first point.
    try testing.expectEqual(FIRE_TEMP_C, field.envTemp(.{ 1.2, 0.0, 0.0 }, 20.0, null));
    try testing.expectEqual(FIRE_TEMP_C, field.envTemp(.{ -10.2, 3.2, 7.7 }, 20.0, null));
    try testing.expect(@abs(field.envTemp(.{ 0.99 + 0.7, 0.0, -0.01 }, 20.0, null) - 210.0) < 0.5);
    // Far from every voxel: air.
    try testing.expectEqual(@as(f32, 20.0), field.envTemp(.{ 3.0, 0.0, 0.0 }, 20.0, null));
    try testing.expectEqual(@as(f32, 20.0), field.envTemp(.{ 0.0, 50.0, 0.0 }, 20.0, null));
    // A 100 C point: lerps 100 -> air between 0.4 m and 1.0 m.
    var hot: FireField = .{};
    defer hot.deinit();
    hot.insert(.{ 0, 0, 0 }, 100.0, null);
    try testing.expect(@abs(hot.envTemp(.{ 0.7, 0.0, 0.0 }, 20.0, null) - 60.0) < 0.5);
}

test "field nearBox rejects far bodies" {
    var field: FireField = .{};
    defer field.deinit();
    try testing.expect(!field.nearBox(.{ 0, 0, 0 }, .{ 1, 1, 1 }));
    field.insert(.{ 10.0, 0.0, 0.0 }, 400.0, null);
    field.insert(.{ 12.0, 2.0, 0.0 }, 400.0, null);
    try testing.expect(field.nearBox(.{ 8.5, 0.0, 0.0 }, .{ 9.5, 1.0, 1.0 }));
    try testing.expect(field.nearBox(.{ 11.0, 1.0, -0.5 }, .{ 11.5, 1.5, 0.5 }));
    try testing.expect(!field.nearBox(.{ 0.0, 0.0, 0.0 }, .{ 8.9, 1.0, 1.0 }));
    try testing.expect(!field.nearBox(.{ 10.0, 0.0, 1.5 }, .{ 11.0, 1.0, 3.0 }));
}

test "source cell excludes itself but warms others" {
    var field: FireField = .{};
    defer field.deinit();
    field.insert(.{ 0, 0, 0 }, 300.0, .{ .body = 1, .cell = .{ 0, 0, 0 } });
    // Own point ignored; another body's cell at the same spot sees it.
    try testing.expectEqual(@as(f32, 20.0), field.envTemp(.{ 0, 0, 0 }, 20.0, .{ .body = 1, .cell = .{ 0, 0, 0 } }));
    try testing.expectEqual(@as(f32, 300.0), field.envTemp(.{ 0, 0, 0 }, 20.0, .{ .body = 2, .cell = .{ 0, 0, 0 } }));
    try testing.expectEqual(@as(f32, 300.0), field.envTemp(.{ 0.25, 0.0, 0.0 }, 20.0, .{ .body = 1, .cell = .{ 1, 0, 0 } }));
}

test "principal axis of a long box" {
    var pts: std.ArrayList([3]f32) = .empty;
    defer pts.deinit(gpa);
    for (0..3) |x| for (0..3) |y| for (0..6) |z| try pts.append(gpa, .{ util.f32_(x), util.f32_(y), util.f32_(z) });
    const pa = principalAxis(pts.items);
    try testing.expect(@abs(pa.axis[2]) > 0.99 and pa.spread > 1.5);
}

test "conducted point skips its own body" {
    var field: FireField = .{};
    defer field.deinit();
    field.insert(.{ 0, 0, 0 }, 200.0, .{ .body = 1, .cell = .{ BODY_WIDE_OWNER, 0, 0 } });
    try testing.expectEqual(@as(f32, 20.0), field.envTemp(.{ 0, 0, 0 }, 20.0, .{ .body = 1, .cell = .{ 3, 0, 0 } }));
    try testing.expectEqual(@as(f32, 200.0), field.envTemp(.{ 0, 0, 0 }, 20.0, .{ .body = 2, .cell = .{ 0, 0, 0 } }));
}
