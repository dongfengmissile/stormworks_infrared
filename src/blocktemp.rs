//! Per-block thermal simulation for the FLIR view.
//!
//! Every voxel cell of every vehicle block carries a temperature (C). The game
//! thread advances the sim from the scene-tick detour; the render thread reads
//! the published per-body grids through [`with_body_grid`].
//!
//! Model (explicit Euler, one lumped temperature per 0.25 m voxel cell):
//!
//! ```text
//! C dT/dt = h_env A (T_env - T) + k sum_n (T_n - T) + solar - h_air A (T - T_air)
//! ```
//!
//! * `T_env` is `T_air` except near a burning voxel: the game's per-body
//!   `compartment_fires` octrees are read directly ([`FireField`]), because the
//!   game's own heat field only models a fire as one 5 m blob at a random burning
//!   cell. Within 0.4 m of a burning voxel it is [`FIRE_TEMP_C`], falling off
//!   linearly to `T_air` at 1.0 m.
//! * `T_air` is the game's ambient base ([`heat::base_ambient_at`]).
//! * `C = mass_per_cell * c_specific`; a component's `def+0x2b0` mass is split
//!   over its voxel cells, so heavy blocks heat and cool slowly.
//! * Conduction runs to the six face neighbours inside the same body and is
//!   symmetric, so it conserves energy.
//! * Solar gain uses the day-gated direction already cached by `sun`/`hooks`
//!   and a constant flux (no per-face normal: one absorbed face per cell).
//!
//! Bodies are interlaced: each one is re-enumerated and stepped only every
//! [`SIM_INTERLACE`] ticks with `dt * SIM_INTERLACE`. A body pays for one
//! ambient probe per update. Fires are re-read every [`FIRE_SCAN_INTERVAL_MS`]
//! with a bounded node budget ([`MAX_FIRE_NODES_PER_SCAN`]).
//! Temperatures persist across ticks and save/load because cells are keyed by
//! the game's stable `(vehicle_id, body_id, voxel)` tuple.
//!
//! All game pointers come from the scene walk and are range/`readable`-checked
//! before use; counts, set sizes and grid dimensions are capped, and anything
//! that fails validation is skipped rather than trusted.

use std::collections::hash_map::Entry;

use crate::fx::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use crate::fault::stage;
use crate::heat;
use crate::ir::Vec3;
use crate::logging::log_once;
use crate::offsets::JET_EXHAUST_PART_TYPE;
use crate::memory::{read_f32_at, read_f64_at, read_u32_at, read_usize_at, readable, PTR_MAX, PTR_MIN};
use crate::offsets::{SCENE_VEHICLE_CONTAINER, SCENE_VEHICLE_SET};
use crate::state;

// ---------------------------------------------------------------------------
// Offsets (facts about stormworks64.exe, see docs/RE_NOTES.md §2)
// ---------------------------------------------------------------------------

/// Vehicle record fields of the scene's record set (see `offsets.rs`).
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
/// Per-body fire log/dump interval (logging feature).
#[cfg(feature = "logging")]
const FIRE_LOG_INTERVAL_MS: u64 = 5000;
/// A source only binds when its published world position is within this many
/// metres of the component's voxel (covers multi-voxel parts and one tick of
/// vehicle motion); otherwise the entry belongs to a different frame.
const SOURCE_MAX_DIST_M: f64 = 4.0;
/// Hottest-cell diagnostics per body (logging feature).
#[cfg(feature = "logging")]
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
/// dimensions against (`flir::body_texture`).
pub(crate) const MAX_GRID_DIM: u32 = 256;
/// Largest accepted set modulus (slot count) of a game set header.
pub(crate) const MAX_SET_MOD: u32 = 1 << 16;
const MAX_VOXEL_COORD: i32 = 1 << 20;

const NEIGHBOURS: [[i32; 3]; 6] = [
    [1, 0, 0],
    [-1, 0, 0],
    [0, 1, 0],
    [0, -1, 0],
    [0, 0, 1],
    [0, 0, -1],
];

// ---------------------------------------------------------------------------
// Public interface (see docs/FLIR_DESIGN.md)
// ---------------------------------------------------------------------------

/// One body's temperature volume, in voxel space (`x` fastest, NaN = empty).
#[derive(Clone)]
pub struct BodyGrid {
    /// Voxel coordinate of texel (0,0,0).
    pub min: [i32; 3],
    /// Texels per axis.
    pub dims: [u32; 3],
    /// Temperatures in Celsius, `len = dims[0]*dims[1]*dims[2]`.
    pub temps_c: Vec<f32>,
    /// Bumped whenever `temps_c` changes (the render thread's upload trigger).
    pub version: u64,
}

/// Advance the block thermal sim by one scene tick. Safe to call every tick;
/// only bodies whose interlace turn it is are re-enumerated, and the fire
/// octrees are re-read at most every [`FIRE_SCAN_INTERVAL_MS`].
pub fn tick(scene: usize) {
    #[cfg(feature = "logging")]
    let t0 = Instant::now();
    // A panic must never unwind through the `extern "C"` scene-tick detour.
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe { tick_inner(scene) }));
    #[cfg(feature = "logging")]
    TICK_COST.add(t0);
}

/// Game-thread CPU cost (sum, max, count in us) for the status line.
#[cfg(feature = "logging")]
pub(crate) struct Cost(
    std::sync::atomic::AtomicU64,
    std::sync::atomic::AtomicU64,
    std::sync::atomic::AtomicU64,
);
#[cfg(feature = "logging")]
impl Cost {
    const fn new() -> Self {
        use std::sync::atomic::AtomicU64;
        Cost(AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0))
    }
    pub(crate) fn add(&self, t0: Instant) {
        let us = t0.elapsed().as_micros() as u64;
        self.0.fetch_add(us, Ordering::Relaxed);
        self.1.fetch_max(us, Ordering::Relaxed);
        self.2.fetch_add(1, Ordering::Relaxed);
    }
    fn take(&self) -> String {
        let (sum, max, n) = (self.0.swap(0, Ordering::Relaxed), self.1.swap(0, Ordering::Relaxed), self.2.swap(0, Ordering::Relaxed));
        format!("n={n} avg={}us max={max}us total={}ms", sum / n.max(1), sum / 1000)
    }
}
#[cfg(feature = "logging")]
static TICK_COST: Cost = Cost::new();
#[cfg(feature = "logging")]
pub(crate) static SCAN_COST: Cost = Cost::new();

/// Vehicles/bodies one scene walk saw (reported by the rate-limited status
/// line, and by anything else that wants the counts).
#[derive(Clone, Copy, Default)]
pub(crate) struct WalkStats {
    pub vehicles: u32,
    pub bodies: u32,
}

/// Borrow the grid of a body for the current frame (key = body pointer).
/// Returns `None` if the body has no published grid. The lock is held for the
/// duration of `f`, which keeps the upload path allocation-free.
pub fn with_body_grid<R>(body: usize, f: impl FnOnce(&BodyGrid) -> R) -> Option<R> {
    let published = lock(&PUBLISHED);
    published.as_ref()?.get(&body).map(f)
}

/// Median ambient air temperature (°C) over the bodies the sim stepped recently
/// (game thread). Bodies colder or warmer than the air around the player cancel
/// out, which makes this a usable scene ambient for a view with no camera of its
/// own (the FLIR main view). `None` when no body has been stepped yet.
pub(crate) fn median_body_ambient() -> Option<f32> {
    median_body(|meta| meta.air_c)
}

/// Median sea-level air temperature under the live bodies.
pub(crate) fn median_ground_ambient() -> Option<f32> {
    median_body(|meta| meta.ground_air_c)
}

fn median_body(field: impl Fn(&BodyMeta) -> f32) -> Option<f32> {
    let now = crate::state::now_ms();
    let sim = lock(&SIM);
    let sim = sim.as_ref()?;
    let mut values: Vec<f32> = sim
        .bodies
        .values()
        .filter(|meta| now.saturating_sub(meta.last_seen_ms) <= 2_000)
        .map(field)
        .filter(|value| value.is_finite())
        .collect();
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(values[values.len() / 2])
}

// ---------------------------------------------------------------------------
// Persistent state
// ---------------------------------------------------------------------------

/// Stable body identity; survives body-pointer churn and save/load.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct BodyKey {
    vehicle_id: u32,
    body_id: u32,
}

/// Stable cell identity: the tuple the game itself stores in net/state records.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct CellKey {
    vehicle_id: u32,
    body_id: u32,
    x: i32,
    y: i32,
    z: i32,
}

struct CellState {
    temp_c: f32,
    /// Lumped heat capacity, J/K (mass split over the component's cells).
    capacity_j_k: f32,
    /// Environment temperature (air or fire) of the last step, C.
    t_env_c: f32,
    /// Last tick the parent body was enumerated (drives stale pruning).
    last_seen_ms: u64,
}

struct BodyMeta {
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
}

/// A component cell while a body grid is being built.
#[derive(Clone, Copy)]
struct RawCell {
    x: i32,
    y: i32,
    z: i32,
    mass_kg: f32,
    /// Owning component pointer (key into the engine source snapshot).
    component: usize,
}

/// A cell of the body currently being stepped, with its texel index.
struct CellSlot {
    index: usize,
    x: i32,
    y: i32,
    z: i32,
    capacity_j_k: f32,
    key: CellKey,
    /// Real component temperature (C) from the plugin's engine sources.
    source_c: Option<f32>,
    /// Owning component pointer.
    component: usize,
}

struct Sim {
    cells: HashMap<CellKey, CellState>,
    bodies: HashMap<BodyKey, BodyMeta>,
    next_version: u64,
    /// Body pointer -> last built grid, reused across ticks (allocations kept).
    staging: HashMap<usize, BodyGrid>,
    /// Body pointers enumerated this tick (stale-grid/publish filter).
    seen: HashSet<usize>,
    /// Body pointers stepped this tick: only these are copied into PUBLISHED.
    stepped: HashSet<usize>,
    /// Scratch buffers, never reallocated per cell.
    pending: Vec<RawCell>,
    slots: Vec<CellSlot>,
    scratch: Vec<f32>,
    /// Burning voxels of all bodies in world space, rebuilt by each fire scan.
    fire: FireField,
    /// Hot source cells per body (world point, temp, owner), refreshed when the body steps.
    heat_src: HashMap<usize, Vec<(Vec3, f32, HeatOwner)>>,
    last_fire_scan_ms: u64,
    /// Bodies whose fire octree layout was already dumped / last fire log time.
    fire_dumped: HashSet<usize>,
    /// Bodies whose raw fire counters (+0x1220 records / +0x1228 default cells) were nonzero last scan.
    fire_bodies_raw: u32,
    fire_log_ms: HashMap<usize, u64>,
    /// Remaining grid bytes this tick may still allocate (see
    /// [`MAX_GRID_BYTES`]); staging already holds most of it across ticks.
    grid_budget: usize,
    /// Cells of all bodies walked in the current tick (status line only).
    tick_cells: usize,
    tick_phase: u32,
    next_phase: u32,
    last_tick: Option<Instant>,
    last_prune_ms: u64,
    /// This tick's fresh engine source temperatures, by component pointer.
    sources: HashMap<usize, (f32, Vec3)>,
    /// Components already logged as bound to a source.
    bound: HashSet<usize>,
}

impl Default for Sim {
    fn default() -> Self {
        Self {
            cells: HashMap::default(),
            bodies: HashMap::default(),
            next_version: 1,
            staging: HashMap::default(),
            seen: HashSet::default(),
            stepped: HashSet::default(),
            pending: Vec::new(),
            slots: Vec::new(),
            scratch: Vec::new(),
            fire: FireField::default(),
            heat_src: HashMap::default(),
            last_fire_scan_ms: 0,
            fire_dumped: HashSet::default(),
            fire_bodies_raw: 0,
            fire_log_ms: HashMap::default(),
            grid_budget: MAX_GRID_BYTES,
            tick_cells: 0,
            tick_phase: 0,
            next_phase: 0,
            last_tick: None,
            last_prune_ms: 0,
            sources: HashMap::default(),
            bound: HashSet::default(),
        }
    }
}

static SIM: Mutex<Option<Sim>> = Mutex::new(None);
/// Grids of the current frame. The render thread only ever reads this map; the
/// sim thread copies in only the bodies it stepped this tick, so a body that is
/// not due keeps exactly the grid it last published.
static PUBLISHED: Mutex<Option<HashMap<usize, BodyGrid>>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// One-shot diagnostic keys (verbose builds only, see `logging::log_once`)
// ---------------------------------------------------------------------------

// `blocktemp` owns bits 0..8 of the shared one-shot word; `flir` owns 8..32.
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
static STATUS_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Set by the first tick that saw at least one vehicle: that tick logs
/// immediately instead of waiting out the interval (the first status line was
/// otherwise easy to miss in a short session).
static STATUS_FIRST_VEHICLE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The first tick that sees a vehicle, then one line every ~5 s: what the scene
/// walk sees, how many burning voxels the last fire scan found and whether the
/// game's heat model has a validated world root. Without it a log that stops at the mesh map
/// cannot tell "sim never ran" from "sim ran and found nothing".
fn report_status(sim: &Sim, stats: WalkStats) {
    #[cfg(feature = "logging")]
    {
        let now_ms = state::now_ms();
        let first_vehicle = stats.vehicles >= 1
            && !STATUS_FIRST_VEHICLE.swap(true, Ordering::Relaxed);
        if !first_vehicle
            && now_ms.saturating_sub(STATUS_MS.load(Ordering::Relaxed)) < STATUS_INTERVAL_MS
        {
            return;
        }
        STATUS_MS.store(now_ms, Ordering::Relaxed);
        let world = unsafe { heat::world() }.is_some();
        crate::logging::log_line(&format!(
            "blocktemp status: vehicles={} bodies={} bodies_tracked={} cells_tick={} \
fire_voxels={} fire_bodies_raw={} cells_tracked={} world_root={} | tick {} | ir.scan {}",
            stats.vehicles,
            stats.bodies,
            sim.bodies.len(),
            sim.tick_cells,
            sim.fire.len,
            sim.fire_bodies_raw,
            sim.cells.len(),
            if world { "ok" } else { "none" },
            TICK_COST.take(),
            SCAN_COST.take(),
        ));
    }
    #[cfg(not(feature = "logging"))]
    let _ = (sim, stats);
}

// ---------------------------------------------------------------------------
// Game-memory helpers
// ---------------------------------------------------------------------------

#[inline]
fn readable_at(address: usize, len: usize) -> bool {
    (PTR_MIN..PTR_MAX).contains(&address) && readable(address, len)
}

/// A game `c_sw_set`-style hash set: `{ T** items; u32 mod; u32 offset; u32 count }`.
#[derive(Clone, Copy)]
pub(crate) struct GameSet {
    items: usize,
    modulo: u32,
    offset: u32,
    count: u32,
}

impl GameSet {
    /// Pointer at logical index `i`; the physical slot is hash-scrambled
    /// (`(i + offset) % mod`), so `i` is never an id. Only used for 8-byte
    /// pointer sets; definition cell sets are indexed by the caller.
    pub(crate) unsafe fn item(&self, i: u32) -> usize {
        let index = (i as usize + self.offset as usize) % self.modulo as usize;
        read_usize_at(self.items + index * 8)
    }

    pub(crate) fn count(&self) -> u32 {
        self.count
    }
}

/// Validate and snapshot a set header. `stride` is the entry size (8 for a
/// pointer set, 60 for a definition's voxel-cell set).
pub(crate) unsafe fn read_set(
    base: usize,
    count_limit: u32,
    stride: usize,
    mod_limit: u32,
) -> Option<GameSet> {
    if !readable_at(base, 0x18) {
        return None;
    }
    let items = read_usize_at(base);
    let modulo = read_u32_at(base + 8);
    let offset = read_u32_at(base + 0xC);
    let count = read_u32_at(base + 0x10);
    if !(PTR_MIN..PTR_MAX).contains(&items)
        || modulo == 0
        || modulo > mod_limit
        || count == 0
        || count > modulo
        || count > count_limit
    {
        return None;
    }
    if !readable(items, modulo as usize * stride) {
        return None;
    }
    Some(GameSet {
        items,
        modulo,
        offset,
        count,
    })
}

/// Row-major float4x4 (translation in row 3) that is finite and non-degenerate.
/// Body voxel-metres -> world as one f64 row-vector matrix: the physics world matrix
/// with its translation pre-shifted by -origin·basis.
unsafe fn body_world_matrix(body: usize) -> Option<[f64; 16]> {
    if !readable_at(body + OFF_BODY_PHYSICS, 8) || !readable_at(body + OFF_BODY_ORIGIN, 12) {
        return None;
    }
    let object = read_usize_at(body + OFF_BODY_PHYSICS);
    if object < PTR_MIN || !readable_at(object + OFF_PHYSICS_WORLD, 16 * 8) {
        return None;
    }
    let mut m = [0f64; 16];
    for (i, value) in m.iter_mut().enumerate() {
        *value = read_f64_at(object + OFF_PHYSICS_WORLD + i * 8);
    }
    let o = [
        read_f32_at(body + OFF_BODY_ORIGIN) as f64,
        read_f32_at(body + OFF_BODY_ORIGIN + 4) as f64,
        read_f32_at(body + OFF_BODY_ORIGIN + 8) as f64,
    ];
    for c in 0..3 {
        m[12 + c] -= o[0] * m[c] + o[1] * m[4 + c] + o[2] * m[8 + c];
    }
    Some(m)
}

fn matrix_ok(m: &[f64; 16]) -> bool {
    if !m.iter().all(|v| v.is_finite()) {
        return false;
    }
    if m[12].abs() > 1.0e6 || m[13].abs() > 1.0e6 || m[14].abs() > 1.0e6 {
        return false;
    }
    // The rotation/scale part must not collapse the voxel frame to a point.
    let row0 = m[0] * m[0] + m[1] * m[1] + m[2] * m[2];
    row0 > 1.0e-6
}

/// Voxel cell -> world metres: `voxel_m * M`, row-vector convention.
#[inline]
fn voxel_to_world(m: &[f64; 16], x: i32, y: i32, z: i32) -> Vec3 {
    let vx = x as f64 * CELL_M as f64;
    let vy = y as f64 * CELL_M as f64;
    let vz = z as f64 * CELL_M as f64;
    (
        vx * m[0] + vy * m[4] + vz * m[8] + m[12],
        vx * m[1] + vy * m[5] + vz * m[9] + m[13],
        vx * m[2] + vy * m[6] + vz * m[10] + m[14],
    )
}

/// The game's ambient base temperature (no heat sources).
unsafe fn air_temperature_at(point: Vec3) -> f32 {
    let t = heat::base_ambient_at(point);
    if t.is_finite() {
        t.clamp(MIN_TEMP_C, MAX_TEMP_C)
    } else {
        DEFAULT_AIR_C
    }
}

/// Texel index of a grid-local voxel: `x` fastest, then `y`, then `z` (the
/// layout `glTexImage3D(width=dims[0], height=dims[1], depth=dims[2])` and the
/// shader's `texelFetch(tex, voxel - grid_min)` read).
#[inline]
fn grid_index(local: [usize; 3], dims: [usize; 3]) -> usize {
    local[2] * dims[0] * dims[1] + local[1] * dims[0] + local[0]
}

/// Hot-gas plumes of this body's jet exhausts in body-local metres (voxel *
/// 0.25, the block mesh's own space): (start, unit direction, radius, excess C).
/// The flow axis runs from the engine's nearest other part (compressor or
/// combustor) to the exhaust part, so no part-rotation convention is needed.
fn jet_plumes(cells: &[RawCell], t_air: f32) -> Vec<([f32; 3], [f32; 3], f32, f32)> {
    let jets: Vec<(usize, u32, f64)> = match state::JET_PARTS.lock() {
        Ok(parts) if !parts.is_empty() => parts.iter().map(|e| (e.0, e.2, e.9)).collect(),
        _ => return Vec::new(),
    };
    // component -> (voxel sum, cells, part type, temp)
    let mut parts: Vec<(usize, [f32; 3], u32, u32, f32)> = Vec::new();
    for raw in cells {
        let Some(&(_, ty, t)) = jets.iter().find(|j| j.0 == raw.component) else {
            continue;
        };
        let v = [raw.x as f32, raw.y as f32, raw.z as f32];
        match parts.iter_mut().find(|p| p.0 == raw.component) {
            Some(p) => {
                for i in 0..3 {
                    p.1[i] += v[i];
                }
                p.2 += 1;
            }
            None => parts.push((raw.component, v, 1, ty, t as f32)),
        }
    }
    let centre = |p: &(usize, [f32; 3], u32, u32, f32)| p.1.map(|c| c / p.2 as f32 * CELL_M);
    let mut out = Vec::new();
    for ex in parts.iter().filter(|p| p.3 == JET_EXHAUST_PART_TYPE) {
        let excess = ex.4 - t_air;
        if !(excess > 50.0) || out.len() >= 8 {
            continue;
        }
        let e = centre(ex);
        let nearest = parts
            .iter()
            .filter(|p| p.3 != JET_EXHAUST_PART_TYPE)
            .map(|p| {
                let c = centre(p);
                let d = [e[0] - c[0], e[1] - c[1], e[2] - c[2]];
                (d, (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt())
            })
            .filter(|(_, len)| *len > 0.05 && *len < 4.0)
            .min_by(|a, b| a.1.total_cmp(&b.1));
        let Some((d, len)) = nearest else {
            log_once(LOG_PLUME_NO_AXIS, || "blocktemp: jet exhaust with no other jet part within 4 m; no plume".to_string());
            continue;
        };
        // A long exhaust (3x3x5) has its flow along its longest axis; the
        // neighbour part only picks the sign (twin engines side by side make
        // the nearest part sit diagonally).
        let mut d = d.map(|c| c / len);
        let (axis, spread) = principal_axis(cells.iter().filter(|c| c.component == ex.0).map(|c| [c.x as f32, c.y as f32, c.z as f32]));
        if spread > 1.5 {
            let s = if axis[0] * d[0] + axis[1] * d[1] + axis[2] * d[2] < 0.0 { -1.0 } else { 1.0 };
            d = axis.map(|c| c * s);
        }
        let len = 1.0;
        // Extent along the flow (cells): the nozzle is the cross-section, the
        // plume starts at the outlet face.
        let proj = |c: &RawCell| c.x as f32 * d[0] + c.y as f32 * d[1] + c.z as f32 * d[2];
        let ex_cells = || cells.iter().filter(|c| c.component == ex.0);
        let lo = ex_cells().map(proj).fold(f32::MAX, f32::min);
        let hi = ex_cells().map(proj).fold(f32::MIN, f32::max);
        let span = (hi - lo + 1.0).max(1.0);
        let e = [0, 1, 2].map(|i| e[i] + d[i] * 0.5 * span * CELL_M);
        let radius = (0.5 * (ex.2 as f32 / span).sqrt() * CELL_M).clamp(0.15, 1.0);
        log_once(LOG_PLUME, || format!(
            "blocktemp: first jet plume start={e:?} m dir=({:.2},{:.2},{:.2}) radius={radius:.2} m excess={excess:.0} C exhaust_cells={}",
            d[0] / len, d[1] / len, d[2] / len, ex.2
        ));
        out.push((e, d.map(|c| c / len), radius, excess));
    }
    out
}

/// Unit longest axis of a point set and how much longer it is than the next
/// (ratio of variances); power iteration on the 3x3 covariance.
fn principal_axis(points: impl Iterator<Item = [f32; 3]>) -> ([f32; 3], f32) {
    let pts: Vec<[f32; 3]> = points.collect();
    if pts.len() < 2 {
        return ([0.0, 0.0, 1.0], 0.0);
    }
    let n = pts.len() as f32;
    let m = [0, 1, 2].map(|i| pts.iter().map(|p| p[i]).sum::<f32>() / n);
    let mut c = [[0.0f32; 3]; 3];
    for p in &pts {
        for i in 0..3 {
            for j in 0..3 {
                c[i][j] += (p[i] - m[i]) * (p[j] - m[j]) / n;
            }
        }
    }
    let mul = |v: [f32; 3]| [0, 1, 2].map(|i| c[i][0] * v[0] + c[i][1] * v[1] + c[i][2] * v[2]);
    let mut v = [0.577, 0.577, 0.577];
    let mut lambda = 0.0;
    for _ in 0..32 {
        let w = mul(v);
        lambda = (w[0] * w[0] + w[1] * w[1] + w[2] * w[2]).sqrt();
        if lambda < 1e-9 {
            return ([0.0, 0.0, 1.0], 0.0);
        }
        v = w.map(|x| x / lambda);
    }
    // Variance across the axis: trace minus the axis' share, split two ways.
    let rest = ((c[0][0] + c[1][1] + c[2][2] - lambda) / 2.0).max(1e-6);
    (v, lambda / rest)
}

/// Per-body position, published on the body's simulation turn, so a seeker
/// can ignore heat sources on its own vehicle.
#[derive(Clone, Copy)]
struct BodyIr {
    stamp_ms: u64,
    /// SCENE_TICKS at publish: game time, unlike wall-clock ms, which bunches
    /// when several sim ticks run in one frame.
    stamp_tick: u64,
    vehicle_id: u32,
    /// Grid-centre world position, its velocity (m/s) and bounding-sphere radius.
    center: Vec3,
    vel: Vec3,
    sphere_r: f64,
}
static BODY_IR: Mutex<Vec<(usize, BodyIr)>> = Mutex::new(Vec::new());
const BODY_IR_STALE_MS: u64 = 2_000;
/// Cells and sources below this world height are under the sea (y = 0), and
/// water is opaque to IR.
pub(crate) const SEA_LEVEL_IR_M: f64 = -0.5;

/// Scene ticks seen by blocktemp (60 per game second).
static SCENE_TICKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const TICK_S: f64 = 1.0 / 60.0;

impl BodyIr {
    /// A body is published once per interlace turn (~4 ticks), so a fast
    /// aircraft would be aimed at where it was; extrapolate to the current tick.
    fn lead(&self, p: Vec3) -> Vec3 {
        let now = SCENE_TICKS.load(std::sync::atomic::Ordering::Relaxed);
        let t = (now.saturating_sub(self.stamp_tick) as f64 * TICK_S).min(0.25);
        (p.0 + self.vel.0 * t, p.1 + self.vel.1 * t, p.2 + self.vel.2 * t)
    }
}

/// Seeker components that have scanned recently: (component, vehicle id once
/// a body step has seen it, last scan ms). Lets a seeker ignore its own vehicle.
static SEEKERS: Mutex<Vec<(usize, Option<u32>, u64)>> = Mutex::new(Vec::new());
const SEEKER_STALE_MS: u64 = 10_000;

fn note_seeker_vehicle(component: usize, vehicle_id: u32) {
    if let Some(e) = lock(&SEEKERS).iter_mut().find(|e| e.0 == component) {
        e.1 = Some(vehicle_id);
    }
}

/// Register `component` as a live seeker and return its vehicle id, once a
/// body step has found the component among that vehicle's cells.
pub(crate) fn seeker_vehicle(component: usize) -> Option<u32> {
    let now = crate::state::now_ms();
    let mut all = lock(&SEEKERS);
    all.retain(|e| now.saturating_sub(e.2) < SEEKER_STALE_MS);
    match all.iter_mut().find(|e| e.0 == component) {
        Some(e) => {
            e.2 = now;
            e.1
        }
        None => {
            if all.len() < 64 {
                all.push((component, None, now));
            }
            None
        }
    }
}

fn publish_body_ir(body: usize, mut e: BodyIr) {
    let mut all = lock(&BODY_IR);
    if let Some(prev) = all.iter().find(|p| p.0 == body) {
        // Wall-clock dt was the bug: two publishes a few ms apart (bunched
        // ticks) turned one tick of motion into thousands of m/s of lead.
        let dt = e.stamp_tick.saturating_sub(prev.1.stamp_tick) as f64 * TICK_S;
        e.vel = prev.1.vel;
        if dt > 0.0 && dt < 1.0 {
            let c = (e.center, prev.1.center);
            let v = ((c.0 .0 - c.1 .0) / dt, (c.0 .1 - c.1 .1) / dt, (c.0 .2 - c.1 .2) / dt);
            // A grid rebuild (damage, detach) moves the centre without motion.
            if (v.0 * v.0 + v.1 * v.1 + v.2 * v.2).sqrt() < 1500.0 {
                e.vel = v;
            }
        }
    }
    let now = e.stamp_ms;
    all.retain(|p| p.0 != body && now.saturating_sub(p.1.stamp_ms) < BODY_IR_STALE_MS);
    let finite = e.vel.0.is_finite() && e.vel.1.is_finite() && e.vel.2.is_finite();
    if finite && all.len() < 512 {
        all.push((body, e));
    }
}

/// Bounding spheres (world centre, radius) of every body of `vehicle_id`, so a
/// seeker can ignore heat sources on its own vehicle.
pub(crate) fn vehicle_spheres(vehicle_id: u32) -> Vec<(Vec3, f64)> {
    let now = crate::state::now_ms();
    lock(&BODY_IR)
        .iter()
        .filter(|(_, e)| e.vehicle_id == vehicle_id && now.saturating_sub(e.stamp_ms) < BODY_IR_STALE_MS)
        .map(|(_, e)| (e.lead(e.center), e.sphere_r))
        .collect()
}

/// Heat-point temperature of an engine exhaust outlet (pipe wall, not gas).
const EXHAUST_PIPE_C: f32 = 250.0;

/// Daylight factor `0..1` from the sun sprite vector cached by the per-tick sun
/// hook (the same game sun `sun.rs` gates on): its normalized height is the
/// sine of the sun's elevation.
fn daylight_factor() -> f32 {
    // The game's own sun (the clock-driven rotation the renderer uses). The
    // per-tick sun-sprite hook never fired in recent runs, which left every
    // vehicle with zero solar gain.
    if let Some((to_sun, _)) = unsafe { crate::sun::game_sun() } {
        return (to_sun[1] as f32).clamp(0.0, 1.0);
    }
    if !state::SUN_DIR_VALID.load(Ordering::Relaxed) {
        return 0.0;
    }
    let dir = *lock(&state::SUN_DIR);
    let len = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2]).sqrt();
    if !len.is_finite() || len <= 1.0e-6 {
        return 0.0;
    }
    (dir[1] / len).clamp(0.0, 1.0) as f32
}

// ---------------------------------------------------------------------------
// Thermal step (pure; the unit tests drive this directly)
// ---------------------------------------------------------------------------

struct StepInputs {
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
    source_c: Option<f32>,
}

/// One explicit-Euler step. The step is shortened when `dt * conductance / C`
/// would exceed [`MAX_STEP_FRACTION`], so a tiny/heavy-mass cell can never
/// overshoot its target.
fn step_temperature(t_c: f32, input: &StepInputs) -> f32 {
    let air = if input.t_air_c.is_finite() {
        input.t_air_c
    } else {
        DEFAULT_AIR_C
    };
    let air = air.clamp(MIN_TEMP_C, MAX_TEMP_C);
    if !t_c.is_finite() || !input.capacity_j_k.is_finite() {
        return air;
    }
    let capacity = input.capacity_j_k.max(MIN_CAPACITY_J_K);
    let neighbours = input.neighbour_count as f32;
    let source = input
        .source_c
        .filter(|t| t.is_finite())
        .map(|t| (t.clamp(MIN_TEMP_C, MAX_TEMP_C), capacity / SOURCE_TAU_S));
    let h_src = source.map_or(0.0, |(_, h)| h);
    let conductance =
        input.h_env_w_k + input.h_air_w_k + input.k_cond_w_k * neighbours + h_src;
    let dt = if conductance > 0.0 {
        input.dt_s.min(MAX_STEP_FRACTION * capacity / conductance)
    } else {
        input.dt_s
    };
    let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
    let heat_w = input.h_env_w_k * (input.t_env_c - t_c)
        + input.h_air_w_k * (air - t_c)
        + input.k_cond_w_k * (input.neighbour_sum_c - neighbours * t_c)
        + input.solar_w
        + source.map_or(0.0, |(t, h)| h * (t - t_c));
    let next = t_c + dt / capacity * heat_w;
    if next.is_finite() {
        next.clamp(MIN_TEMP_C, MAX_TEMP_C)
    } else {
        air
    }
}

// ---------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------

/// One component definition's cell data. A vehicle reuses a few hundred
/// definitions across thousands of blocks, and every miss costs a handful of
/// `readable` (VirtualQuery) calls, so it is read once per body walk.
#[derive(Clone, Copy)]
struct DefCells {
    cells: Option<GameSet>,
    /// Mass of one voxel cell, kg (definition mass split over the cell set).
    per_cell_kg: f32,
}

/// Read a definition's cell set and split its mass over the cells.
unsafe fn read_definition(definition: usize) -> DefCells {
    let mass = if readable_at(definition, OFF_DEF_MASS + 4) {
        let mass = read_f32_at(definition + OFF_DEF_MASS);
        if mass.is_finite() && mass > 0.0 {
            mass
        } else {
            0.0
        }
    } else {
        0.0
    };
    let cells = read_set(
        definition + OFF_DEF_CELLS,
        MAX_CELLS_PER_COMPONENT,
        CELL_ENTRY_BYTES,
        MAX_SET_MOD,
    );
    let per_cell_kg = match cells {
        Some(set) if set.count > 0 => {
            let per_cell = if mass > 0.0 {
                mass / set.count as f32
            } else {
                FALLBACK_MASS_KG_PER_CELL
            };
            if per_cell.is_finite() && per_cell > 0.0 {
                per_cell
            } else {
                FALLBACK_MASS_KG_PER_CELL
            }
        }
        // No readable cell set (or a usable mass without one): one voxel.
        _ if mass > 0.0 => mass,
        _ => FALLBACK_MASS_KG_PER_CELL,
    };
    DefCells { cells, per_cell_kg }
}

/// Read one body's components into `out` as voxel cells, splitting each
/// component's mass over its cells. Returns false only when the component set
/// itself is unreadable.
unsafe fn collect_cells(body_ptr: usize, out: &mut Vec<RawCell>) -> (bool, u32) {
    let components = match read_set(
        body_ptr + OFF_BODY_COMPONENTS,
        MAX_COMPONENTS_PER_BODY,
        8,
        MAX_SET_MOD,
    ) {
        Some(set) => set,
        None => return (false, 0),
    };
    let mut components_read = 0u32;
    let mut defs: HashMap<usize, DefCells> = HashMap::default();
    // The cap counts VISITED cells, not pushed ones: a cell whose voxel offset
    // is out of range is skipped without growing `out`, so `out.len()` alone
    // would let a garbage set drive ~1e9 iterations per body.
    let mut visited = out.len();
    for i in 0..components.count {
        if visited >= MAX_CELLS_PER_BODY {
            break;
        }
        let component = components.item(i);
        if !readable_at(component, OFF_COMP_DEF + 8) {
            continue;
        }
        components_read += 1;
        let definition = read_usize_at(component + OFF_COMP_DEF);
        let origin = [
            read_u32_at(component + OFF_COMP_VOXEL) as i32,
            read_u32_at(component + OFF_COMP_VOXEL + 4) as i32,
            read_u32_at(component + OFF_COMP_VOXEL + 8) as i32,
        ];
        if origin
            .iter()
            .any(|v| v.unsigned_abs() > MAX_VOXEL_COORD as u32)
        {
            continue;
        }

        // Voxel-space rotation: axis permutations only; anything else means the
        // pointer was not the component we expected.
        let mut rotation = [1i32, 0, 0, 0, 1, 0, 0, 0, 1];
        let mut rotation_ok = false;
        if readable_at(component + OFF_COMP_ROT, 9 * 4) {
            rotation_ok = true;
            for (k, slot) in rotation.iter_mut().enumerate() {
                let value = read_u32_at(component + OFF_COMP_ROT + k * 4) as i32;
                if !(-1..=1).contains(&value) {
                    rotation_ok = false;
                    break;
                }
                *slot = value;
            }
        }
        // A voxel rotation is a signed permutation: one +-1 per row and column.
        // Anything else (a zero row) would collapse a multi-voxel part's cells
        // onto a line - e.g. only the middle row of a 3x3 radiator.
        rotation_ok = rotation_ok
            && (0..3).all(|r| (0..3).filter(|&c| rotation[r * 3 + c] != 0).count() == 1)
            && (0..3).all(|c| (0..3).filter(|&r| rotation[r * 3 + c] != 0).count() == 1);
        if !rotation_ok {
            rotation = [1, 0, 0, 0, 1, 0, 0, 0, 1];
        }

        let def = match defs.get(&definition) {
            Some(def) => *def,
            None => {
                let def = read_definition(definition);
                defs.insert(definition, def);
                def
            }
        };

        match def.cells {
            Some(set) if set.count > 0 => {
                let first_index = set.offset as usize % set.modulo as usize;
                let first_entry = set.items + first_index * CELL_ENTRY_BYTES;
                log_once(LOG_SET, || {
                    format!(
                        "blocktemp: first cell set def={definition:#x} count={} mod={} offset={} \
origin=({origin:?}) rotation={rotation:?} first_offset=({},{},{})",
                        set.count,
                        set.modulo,
                        set.offset,
                        read_u32_at(first_entry) as i32,
                        read_u32_at(first_entry + 4) as i32,
                        read_u32_at(first_entry + 8) as i32,
                    )
                });
                #[cfg(feature = "logging")]
                if set.count > 1 {
                    // Multi-voxel parts are where rotation matters: one line per
                    // definition (first 24) to check cells against the part.
                    static SEEN: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
                    let mut seen = SEEN.lock().unwrap_or_else(|p| p.into_inner());
                    if seen.len() < 24 && !seen.contains(&definition) {
                        seen.push(definition);
                        drop(seen);
                        let cells: Vec<[i32; 3]> = (0..set.count.min(12))
                            .map(|k| {
                                let e = set.items + ((k as usize + set.offset as usize) % set.modulo as usize) * CELL_ENTRY_BYTES;
                                [read_u32_at(e) as i32, read_u32_at(e + 4) as i32, read_u32_at(e + 8) as i32]
                            })
                            .collect();
                        crate::logging::log_line(&format!(
                            "blocktemp: multi-voxel part def={definition:#x} comp={component:#x} count={} \
origin={origin:?} rotation={rotation:?} (read_ok={rotation_ok}) cells={cells:?}",
                            set.count
                        ));
                    }
                }
                let per_cell = def.per_cell_kg;
                for cell_index in 0..set.count {
                    if visited >= MAX_CELLS_PER_BODY {
                        break;
                    }
                    visited += 1;
                    let index = (cell_index as usize + set.offset as usize) % set.modulo as usize;
                    let entry = set.items + index * CELL_ENTRY_BYTES;
                    let offset = [
                        read_u32_at(entry) as i32,
                        read_u32_at(entry + 4) as i32,
                        read_u32_at(entry + 8) as i32,
                    ];
                    // Reject out-of-range offsets BEFORE rotating: the rotation
                    // terms are `i32 * offset`, and an offset near i32::MIN would
                    // overflow (a debug-build panic here would be caught by the
                    // tick's catch_unwind and re-panic every interlaced turn).
                    if offset
                        .iter()
                        .any(|v| v.unsigned_abs() > MAX_VOXEL_COORD as u32)
                    {
                        continue;
                    }
                    let rotated = [
                        rotation[0] * offset[0] + rotation[1] * offset[1] + rotation[2] * offset[2],
                        rotation[3] * offset[0] + rotation[4] * offset[1] + rotation[5] * offset[2],
                        rotation[6] * offset[0] + rotation[7] * offset[1] + rotation[8] * offset[2],
                    ];
                    let x = origin[0].saturating_add(rotated[0]);
                    let y = origin[1].saturating_add(rotated[1]);
                    let z = origin[2].saturating_add(rotated[2]);
                    if [x, y, z]
                        .iter()
                        .any(|v| v.unsigned_abs() > MAX_VOXEL_COORD as u32)
                    {
                        continue;
                    }
                    out.push(RawCell {
                        x,
                        y,
                        z,
                        mass_kg: per_cell,
                        component,
                    });
                }
            }
            _ => {
                // No readable cell set: treat the component as one voxel. This
                // keeps plain 1x1x1 blocks working even if their definition set
                // layout ever changes.
                visited += 1;
                out.push(RawCell {
                    x: origin[0],
                    y: origin[1],
                    z: origin[2],
                    mass_kg: def.per_cell_kg,
                    component,
                });
            }
        }
    }
    (true, components_read)
}

/// Inclusive voxel bounds, or `None` when the body exceeds the grid caps.
fn grid_bounds(cells: &[RawCell]) -> Option<([i32; 3], [u32; 3], usize)> {
    if cells.is_empty() {
        return None;
    }
    let mut min = [i32::MAX; 3];
    let mut max = [i32::MIN; 3];
    for cell in cells {
        let coordinates = [cell.x, cell.y, cell.z];
        for axis in 0..3 {
            min[axis] = min[axis].min(coordinates[axis]);
            max[axis] = max[axis].max(coordinates[axis]);
        }
    }
    let mut dims = [0u32; 3];
    let mut total: usize = 1;
    for axis in 0..3 {
        let extent = (max[axis] as i64 - min[axis] as i64 + 1) as u64;
        if extent == 0 || extent > MAX_GRID_DIM as u64 {
            return None;
        }
        dims[axis] = extent as u32;
        total = total.checked_mul(extent as usize)?;
    }
    if total == 0 || total > MAX_CELLS_PER_BODY {
        return None;
    }
    Some((min, dims, total))
}

// ---------------------------------------------------------------------------
// Tick
// ---------------------------------------------------------------------------

/// Walk the scene's vehicles and their bodies, yielding `(vehicle_id, body)`.
///
/// The one copy of the scene -> vehicles -> bodies container walk (the sim and
/// the render side's mesh map both use it): every level is `read`-guarded and
/// clamped, and a body is only yielded when its id field is readable. `f`
/// returns false to stop the walk early.
pub(crate) unsafe fn for_each_body(scene: usize, mut f: impl FnMut(u32, usize) -> bool) -> WalkStats {
    let mut stats = WalkStats::default();
    if scene < PTR_MIN || !readable(scene + SCENE_VEHICLE_CONTAINER, 0x18) {
        return stats;
    }
    let container = scene + SCENE_VEHICLE_CONTAINER;
    let Some(records) = read_set(container + SCENE_VEHICLE_SET, MAX_VEHICLES, 8, MAX_SET_MOD)
    else {
        return stats;
    };
    let mut bodies_seen = 0usize;
    for i in 0..records.count {
        let record = records.item(i);
        if !readable_at(record, 0x14) {
            continue;
        }
        if *(record as *const u8) != 0 {
            continue; // removed/inactive record
        }
        if read_u32_at(record + OFF_REC_TYPE) != REC_TYPE_VEHICLE {
            continue;
        }
        let vehicle_id = read_u32_at(record + OFF_REC_VEHICLE_ID);
        let vehicle = read_usize_at(record + OFF_REC_VEHICLE);
        if !readable_at(vehicle, 0x18) {
            continue;
        }
        let Some(bodies) = read_set(
            vehicle + OFF_VEHICLE_BODIES,
            MAX_BODIES_PER_VEHICLE,
            8,
            MAX_SET_MOD,
        ) else {
            continue;
        };
        stats.vehicles += 1;
        for j in 0..bodies.count {
            if bodies_seen >= MAX_BODIES_TOTAL {
                return stats;
            }
            let body = bodies.item(j);
            if !readable_at(body, OFF_BODY_ID + 4) {
                continue;
            }
            bodies_seen += 1;
            stats.bodies += 1;
            if !f(vehicle_id, body) {
                return stats;
            }
        }
    }
    stats
}

unsafe fn tick_inner(scene: usize) {
    let _stage = crate::fault::enter(stage::BLOCKTEMP_TICK);
    SCENE_TICKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let now_ms = state::now_ms();
    let now = Instant::now();
    let sun = daylight_factor();

    // Snapshot fresh engine sources first; no lock is held during the sim.
    let fresh: HashMap<usize, (f32, Vec3)> = crate::state::EMITTER_PENDING
        .lock()
        .map(|pending| {
            pending
                .iter()
                .filter(|e| now_ms.saturating_sub(e.1) <= SOURCE_FRESH_MS)
                .filter(|e| e.6.is_finite() && e.6 > 0.0)
                .map(|e| (e.0, (e.6 as f32, (e.2, e.3, e.4))))
                .collect()
        })
        .unwrap_or_default();

    let mut guard = lock(&SIM);
    let sim = guard.get_or_insert_with(Sim::default);
    sim.bound.retain(|c| fresh.contains_key(c));
    sim.sources = fresh;
    let (dt, tick_phase) = sim.begin_tick(now);
    sim.seen.clear();
    sim.stepped.clear();
    sim.scan_fires(scene, now_ms);

    crate::fault::set(stage::BLOCKTEMP_ENUMERATE);
    let stats = for_each_body(scene, |vehicle_id, body| {
        sim.process_body(body, vehicle_id, now_ms, dt, sun, tick_phase);
        true
    });

    crate::fault::set(stage::BLOCKTEMP_PUBLISH);
    sim.publish(now_ms);
    report_status(sim, stats);
}

// ---------------------------------------------------------------------------
// Fire field: burning voxels in world space
// ---------------------------------------------------------------------------

/// `T_env` for a cell at distance `d` metres from the nearest burning voxel.
fn fire_temp_at(d: f64, t_air: f32) -> f32 {
    heat_temp_at(d, FIRE_TEMP_C, t_air)
}

/// Point at `point_c`: full within `FIRE_FULL_M`, linear to `t_air` at `FIRE_FADE_M`.
fn heat_temp_at(d: f64, point_c: f32, t_air: f32) -> f32 {
    let k = ((FIRE_FADE_M - d) / (FIRE_FADE_M - FIRE_FULL_M)).clamp(0.0, 1.0) as f32;
    t_air.max(t_air + (point_c - t_air) * k)
}

/// Owner of a heat point (body pointer + cell), so a source cell can skip itself.
type HeatOwner = (usize, [i32; 3]);
/// Owner voxel x of a conducted heat point: it heats every body but its own.
const BODY_WIDE_OWNER: i32 = i32::MIN;
const CONDUCTED_POINT_EXCESS_C: f32 = 15.0;
const MAX_CONDUCTED_POINTS: usize = 2048;

/// World-space heat points (burning voxels and hot source cells) in a spatial hash.
#[derive(Default)]
struct FireField {
    buckets: HashMap<[i32; 3], Vec<(Vec3, f32, Option<HeatOwner>)>>,
    len: usize,
    /// World bounds of all points (valid when `len > 0`), for a per-body reject.
    lo: Vec3,
    hi: Vec3,
}

impl FireField {
    fn bucket(p: Vec3) -> [i32; 3] {
        [
            (p.0 / FIRE_BUCKET_M).floor() as i32,
            (p.1 / FIRE_BUCKET_M).floor() as i32,
            (p.2 / FIRE_BUCKET_M).floor() as i32,
        ]
    }

    fn insert(&mut self, p: Vec3, temp_c: f32, owner: Option<HeatOwner>) {
        if self.len >= MAX_HEAT_POINTS
            || !temp_c.is_finite()
            || !(p.0.is_finite() && p.1.is_finite() && p.2.is_finite())
            || p.0.abs() > 1.0e6
            || p.1.abs() > 1.0e6
            || p.2.abs() > 1.0e6
        {
            return;
        }
        self.buckets
            .entry(Self::bucket(p))
            .or_default()
            .push((p, temp_c, owner));
        if self.len == 0 {
            self.lo = p;
            self.hi = p;
        } else {
            self.lo = (self.lo.0.min(p.0), self.lo.1.min(p.1), self.lo.2.min(p.2));
            self.hi = (self.hi.0.max(p.0), self.hi.1.max(p.1), self.hi.2.max(p.2));
        }
        self.len += 1;
    }

    /// True when a box (world) comes within `FIRE_FADE_M` of any point's bounds.
    fn near_box(&self, lo: Vec3, hi: Vec3) -> bool {
        let r = FIRE_FADE_M;
        self.len > 0
            && lo.0 <= self.hi.0 + r
            && hi.0 >= self.lo.0 - r
            && lo.1 <= self.hi.1 + r
            && hi.1 >= self.lo.1 - r
            && lo.2 <= self.hi.2 + r
            && hi.2 >= self.lo.2 - r
    }

    /// Hottest contribution within `FIRE_FADE_M`, never below `t_air`; the
    /// point owned by `own` is ignored.
    fn env_temp(&self, p: Vec3, t_air: f32, own: Option<HeatOwner>) -> f32 {
        let [bx, by, bz] = Self::bucket(p);
        let mut best = t_air;
        for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    let Some(points) = self.buckets.get(&[bx + dx, by + dy, bz + dz]) else {
                        continue;
                    };
                    for &(q, t, owner) in points {
                        if own.is_some() && owner == own {
                            continue;
                        }
                        if let (Some(o), Some(w)) = (owner, own) {
                            if o.1[0] == BODY_WIDE_OWNER && o.0 == w.0 {
                                continue;
                            }
                        }
                        let d = ((p.0 - q.0).powi(2) + (p.1 - q.1).powi(2) + (p.2 - q.2).powi(2))
                            .sqrt();
                        if d < FIRE_FADE_M {
                            best = best.max(heat_temp_at(d, t, t_air));
                        }
                    }
                }
            }
        }
        best
    }
}

/// One burning cell read from a body's fire octree.
#[derive(Clone, Copy)]
struct FireVoxel {
    pos: [i32; 3],
    active: bool,
    /// Decided by the slot vector (true) or by mere presence in the octree.
    by_slots: bool,
}

#[derive(Default)]
struct FireScan {
    records: u32,
    cells_field: i64,
    leaves: u32,
    rejected: u32,
    inner: u32,
    by_slots: u32,
    by_presence: u32,
}

#[inline]
fn aligned_ptr(p: usize) -> bool {
    p & 7 == 0 && (PTR_MIN..PTR_MAX).contains(&p)
}

/// Is any slot of this leaf active? `None` if the slot vector is unreadable/empty.
unsafe fn leaf_active(leaf: usize) -> Option<bool> {
    let count = read_u32_at(leaf + OFF_LEAF_COUNT) as usize;
    let cap = read_u32_at(leaf + OFF_LEAF_CAP) as usize;
    let start = read_u32_at(leaf + OFF_LEAF_START) as usize;
    let items = read_usize_at(leaf + OFF_LEAF_SLOTS);
    if count == 0 || count > 64 || cap == 0 || cap > 4096 || count > cap || !aligned_ptr(items) {
        return None;
    }
    if !readable(items, cap * LEAF_SLOT_BYTES) {
        return None;
    }
    let mut any = false;
    for i in 0..count {
        let idx = (start + i) % cap;
        any |= *((items + idx * LEAF_SLOT_BYTES + OFF_SLOT_ACTIVE) as *const u8) != 0;
    }
    Some(any)
}

/// Walk a body's fire records (`body+0x1218` x `body+0x1220`, then the inline
/// default record at `body+0x1228`). Octree per record: 8 root slots at +0x08,
/// each an octant of `[-512,512)^3`; nodes halve until size 2, whose slots point
/// at 8-byte boxes holding the leaf pointer. A leaf's int3 cell (+0x18) must lie
/// in the cube its path describes. `dump` receives raw leaf bytes (logging).
unsafe fn collect_fire_voxels(
    body: usize,
    budget: &mut u32,
    out: &mut Vec<FireVoxel>,
    mut dump: Option<&mut Vec<String>>,
) -> FireScan {
    let mut scan = FireScan::default();
    let mut recs: Vec<usize> = Vec::new();
    if readable_at(body + OFF_BODY_FIRES, 8) && readable_at(body + OFF_BODY_FIRE_COUNT, 4) {
        let base = read_usize_at(body + OFF_BODY_FIRES);
        let count = read_u32_at(body + OFF_BODY_FIRE_COUNT);
        if count != 0
            && count <= MAX_FIRE_RECORDS
            && aligned_ptr(base)
            && readable(base, count as usize * FIRE_RECORD_BYTES)
        {
            recs.extend((0..count as usize).map(|r| base + r * FIRE_RECORD_BYTES));
        }
    }
    if readable_at(body + OFF_BODY_DEFAULT_FIRE, FIRE_RECORD_BYTES) {
        recs.push(body + OFF_BODY_DEFAULT_FIRE);
    }
    // (node, cube min, cube edge)
    let mut stack: Vec<(usize, [i32; 3], i32)> = Vec::new();
    for rec in recs {
        let cells = read_u32_at(rec + OFF_FIRE_CELLS) as i32;
        if cells == 0 {
            continue;
        }
        scan.records += 1;
        scan.cells_field += cells as i64;
        for o in 0..8usize {
            let root = read_usize_at(rec + OFF_FIRE_ROOTS + o * 8);
            if root == 0 {
                continue;
            }
            let half = FIRE_HALF;
            let min = [
                -half + if o & 1 != 0 { half } else { 0 },
                -half + if o & 2 != 0 { half } else { 0 },
                -half + if o & 4 != 0 { half } else { 0 },
            ];
            stack.push((root, min, half));
            while let Some((node, cmin, size)) = stack.pop() {
                if *budget == 0 || out.len() >= MAX_FIRE_VOXELS {
                    return scan;
                }
                *budget -= 1;
                if !aligned_ptr(node) || !readable(node, NODE_BYTES) {
                    scan.rejected += 1;
                    continue;
                }
                scan.inner += 1;
                let h = size / 2;
                for c in 0..8usize {
                    let child = read_usize_at(node + c * 8);
                    if child == 0 {
                        continue;
                    }
                    let cm = [
                        cmin[0] + if c & 1 != 0 { h } else { 0 },
                        cmin[1] + if c & 2 != 0 { h } else { 0 },
                        cmin[2] + if c & 4 != 0 { h } else { 0 },
                    ];
                    if size > 2 {
                        stack.push((child, cm, h));
                        continue;
                    }
                    // size == 2: child is a box holding the leaf pointer.
                    if *budget == 0 {
                        return scan;
                    }
                    *budget -= 1;
                    if !aligned_ptr(child) || !readable(child, 8) {
                        scan.rejected += 1;
                        continue;
                    }
                    let leaf = read_usize_at(child);
                    if !aligned_ptr(leaf) || !readable(leaf, 0x28) {
                        scan.rejected += 1;
                        continue;
                    }
                    let pos = [
                        read_u32_at(leaf + OFF_NODE_POS) as i32,
                        read_u32_at(leaf + OFF_NODE_POS + 4) as i32,
                        read_u32_at(leaf + OFF_NODE_POS + 8) as i32,
                    ];
                    let inside = (0..3).all(|i| pos[i] >= cm[i] && pos[i] < cm[i] + h);
                    let slots = leaf_active(leaf);
                    let (active, by_slots) = match slots {
                        Some(a) => (a, true),
                        None => (true, false),
                    };
                    if let Some(lines) = dump.as_deref_mut() {
                        if lines.len() < 16 {
                            let mut raw = String::new();
                            for i in 0..0x28 {
                                raw.push_str(&format!("{:02x}", *((leaf + i) as *const u8)));
                            }
                            lines.push(format!(
                                "leaf={leaf:#x} cube_min={cm:?} voxel={pos:?} inside={inside} \
active={active} rule={} raw={raw}",
                                if by_slots { "slot_flag" } else { "presence" }
                            ));
                        }
                    }
                    if !inside {
                        scan.rejected += 1;
                        continue;
                    }
                    scan.leaves += 1;
                    if by_slots {
                        scan.by_slots += 1;
                    } else {
                        scan.by_presence += 1;
                    }
                    out.push(FireVoxel {
                        pos,
                        active,
                        by_slots,
                    });
                }
            }
        }
    }
    scan
}

impl Sim {
    /// Re-read every body's fire octrees into the world-space [`FireField`].
    /// Bodies keep their last field only until the next scan replaces it.
    unsafe fn scan_fires(&mut self, scene: usize, now_ms: u64) {
        if now_ms.saturating_sub(self.last_fire_scan_ms) < FIRE_SCAN_INTERVAL_MS {
            return;
        }
        self.last_fire_scan_ms = now_ms;
        crate::fault::set(stage::BLOCKTEMP_ENUMERATE);
        let mut field = FireField::default();
        let mut budget = MAX_FIRE_NODES_PER_SCAN;
        let mut voxels: Vec<FireVoxel> = Vec::new();
        let mut raw_bodies = 0u32;
        let staging = &self.staging;
        let dumped = &mut self.fire_dumped;
        let log_ms = &mut self.fire_log_ms;
        #[cfg(not(feature = "logging"))]
        let _ = (&dumped, &log_ms);
        for_each_body(scene, |_vehicle_id, body| {
            let Some(matrix) = body_world_matrix(body) else {
                return true;
            };
            if !matrix_ok(&matrix) {
                return true;
            }
            voxels.clear();
            if (readable_at(body + OFF_BODY_FIRE_COUNT, 4) && read_u32_at(body + OFF_BODY_FIRE_COUNT) != 0)
                || (readable_at(body + OFF_BODY_DEFAULT_FIRE, 4)
                    && read_u32_at(body + OFF_BODY_DEFAULT_FIRE) != 0)
            {
                raw_bodies += 1;
            }
            #[cfg(feature = "logging")]
            let mut dump_lines: Vec<String> = Vec::new();
            #[cfg(feature = "logging")]
            let want_dump = !dumped.contains(&body)
                && readable_at(body + OFF_BODY_FIRE_COUNT, 4)
                && readable_at(body + OFF_BODY_DEFAULT_FIRE, 4)
                && (read_u32_at(body + OFF_BODY_FIRE_COUNT) != 0
                    || read_u32_at(body + OFF_BODY_DEFAULT_FIRE) != 0);
            #[cfg(feature = "logging")]
            let dump = if want_dump { Some(&mut dump_lines) } else { None };
            #[cfg(not(feature = "logging"))]
            let dump = None;
            let scan = collect_fire_voxels(body, &mut budget, &mut voxels, dump);
            // Plausible range: the body's known cell box plus a margin.
            let known = staging.get(&body).map(|g| {
                let lo = g.min;
                let hi = [
                    g.min[0] + g.dims[0] as i32,
                    g.min[1] + g.dims[1] as i32,
                    g.min[2] + g.dims[2] as i32,
                ];
                (lo, hi)
            });
            let mut kept = 0u32;
            let mut lo = [i32::MAX; 3];
            let mut hi = [i32::MIN; 3];
            for v in voxels.iter().filter(|v| v.active) {
                if let Some((klo, khi)) = known {
                    if (0..3).any(|i| {
                        v.pos[i] < klo[i] - FIRE_BOX_MARGIN || v.pos[i] > khi[i] + FIRE_BOX_MARGIN
                    }) {
                        continue;
                    }
                }
                for i in 0..3 {
                    lo[i] = lo[i].min(v.pos[i]);
                    hi[i] = hi[i].max(v.pos[i]);
                }
                kept += 1;
                field.insert(
                    voxel_to_world(&matrix, v.pos[0], v.pos[1], v.pos[2]),
                    FIRE_TEMP_C,
                    None,
                );
            }
            #[cfg(feature = "logging")]
            {
                if !dump_lines.is_empty() {
                    dumped.insert(body);
                    crate::logging::log_line(&format!(
                        "blocktemp: fire dump body={body:#x} records={} cells_field={} leaves={} inner={} \\
rejected={} active_rule: slot_flag={} presence_fallback={} (leaf = int3 voxel at +0x18 inside \\
the path cube; slot_flag = any slot active byte set, presence = slot vector unreadable)",
                        scan.records, scan.cells_field, scan.leaves, scan.inner, scan.rejected,
                        scan.by_slots, scan.by_presence
                    ));
                    for line in &dump_lines {
                        crate::logging::log_line(&format!("blocktemp: fire dump {line}"));
                    }
                }
                if kept > 0 {
                    let last = log_ms.get(&body).copied().unwrap_or(0);
                    if now_ms.saturating_sub(last) >= FIRE_LOG_INTERVAL_MS {
                        log_ms.insert(body, now_ms);
                        crate::logging::log_line(&format!(
                            "blocktemp: fire body={body:#x} burning_voxels={kept} of {} leaves \\
(cells_field={}) bbox={lo:?}..{hi:?}",
                            scan.leaves, scan.cells_field
                        ));
                    }
                }
            }
            #[cfg(not(feature = "logging"))]
            let _ = (kept, lo, hi, scan);
            true
        });
        self.fire_bodies_raw = raw_bodies;
        // Hot engine/jet/radiator cells radiate to every body (rebuilt each scan).
        self.heat_src.retain(|body, _| self.staging.contains_key(body));
        for points in self.heat_src.values() {
            for &(p, t, owner) in points {
                field.insert(p, t, Some(owner));
            }
        }
        // Exhaust outlets: the plume emitter sits at the pipe end, so the last
        // blocks of an exhaust run heat up (capped: the gas is hotter than the
        // pipe wall it touches).
        if let Ok(keys) = state::EXHAUST_EMITTERS.lock() {
            for (key, &(t, pos)) in &self.sources {
                if keys.contains(key) {
                    field.insert(pos, t.min(EXHAUST_PIPE_C), None);
                }
            }
        }
        self.fire = field;
    }
}

impl Sim {
    /// Seconds this tick represents (scaled by the interlace factor) and the
    /// phase whose bodies are due.
    fn begin_tick(&mut self, now: Instant) -> (f32, u32) {
        let raw = match self.last_tick.replace(now) {
            Some(previous) => now.saturating_duration_since(previous).as_secs_f32(),
            None => DEFAULT_TICK_DT_S,
        };
        let dt = if raw.is_finite() && raw > 0.0 {
            raw.min(MAX_TICK_DT_S)
        } else {
            DEFAULT_TICK_DT_S
        };
        let phase = self.tick_phase;
        self.tick_phase = (self.tick_phase + 1) % SIM_INTERLACE;
        self.tick_cells = 0;
        // Grids persist across ticks (allocations are reused), so only what is
        // left after the staging map's current size is available to new bodies.
        let held: usize = self
            .staging
            .values()
            .map(|grid| grid.temps_c.len() * std::mem::size_of::<f32>())
            .sum();
        self.grid_budget = MAX_GRID_BYTES.saturating_sub(held);
        (dt * SIM_INTERLACE as f32, phase)
    }

    /// Advance one body: re-enumerate it on its interlace turn, keep its
    /// published grid alive otherwise.
    unsafe fn process_body(
        &mut self,
        body_ptr: usize,
        vehicle_id: u32,
        now_ms: u64,
        dt: f32,
        sun: f32,
        tick_phase: u32,
    ) {
        crate::fault::set(stage::BLOCKTEMP_COLLECT_CELLS);
        let body_id = read_u32_at(body_ptr + OFF_BODY_ID);
        let body_key = BodyKey {
            vehicle_id,
            body_id,
        };

        // Interlace phase is assigned once per body identity and stays stable.
        let phase = match self.bodies.get(&body_key) {
            Some(meta) => meta.phase,
            None => {
                let phase = self.next_phase % SIM_INTERLACE;
                self.next_phase = self.next_phase.wrapping_add(1);
                self.bodies.insert(
                    body_key,
                    BodyMeta {
                        last_seen_ms: now_ms,
                        phase,
                        air_c: f32::NAN,
                        ground_air_c: f32::NAN,
                        last_hot_log_ms: 0,
                    },
                );
                phase
            }
        };
        if let Some(meta) = self.bodies.get_mut(&body_key) {
            meta.last_seen_ms = now_ms;
        }

        // Only a body's interlace turn re-enumerates and steps it; every other
        // tick just keeps its last published grid alive. A body without a grid
        // yet is always built so it appears on its first sighting.
        if phase != tick_phase && self.staging.contains_key(&body_ptr) {
            self.seen.insert(body_ptr);
            return;
        }

        let Some(matrix) = body_world_matrix(body_ptr) else {
            log_once(LOG_MATRIX, || {
                format!("blocktemp: body {body_ptr:#x} has no physics object, skipped")
            });
            return;
        };
        if !matrix_ok(&matrix) {
            log_once(LOG_MATRIX, || {
                format!("blocktemp: body {body_ptr:#x} has an invalid matrix, skipped")
            });
            return;
        }

        // One ambient probe per body update, not per cell.
        let origin = (matrix[12], matrix[13], matrix[14]);

        self.pending.clear();
        let (component_set_ok, components_read) = collect_cells(body_ptr, &mut self.pending);
        if !component_set_ok || self.pending.is_empty() {
            return;
        }

        let Some((min, dims, total)) = grid_bounds(&self.pending) else {
            log_once(LOG_SKIP, || {
                format!(
                    "blocktemp: body {body_ptr:#x} exceeds grid caps (cells={}), skipped",
                    self.pending.len()
                )
            });
            return;
        };

        // Grid memory budget: a giant spawn (or a garbage count) must not turn
        // into gigabytes of f32 grids. Bodies beyond the budget are skipped
        // (and so dropped from the published map) rather than truncated.
        let grid_bytes = total * std::mem::size_of::<f32>();
        self.tick_cells += total;
        let existing = self
            .staging
            .get(&body_ptr)
            .map_or(0, |grid| grid.temps_c.len() * std::mem::size_of::<f32>());
        let growth = grid_bytes.saturating_sub(existing);
        if growth > self.grid_budget {
            log_once(LOG_BUDGET, || {
                format!(
                    "blocktemp: grid budget ({} MB) exhausted; body {body_ptr:#x} with {total} cells skipped",
                    MAX_GRID_BYTES >> 20
                )
            });
            return;
        }
        self.grid_budget -= growth;

        log_once(LOG_CELLS, || {
            let first = self.pending[0];
            format!(
                "blocktemp: body {body_ptr:#x} vid={vehicle_id} bid={body_id} components={components_read} \
cells={} min={min:?} dims={dims:?} first_cell=({},{},{}) d0={:.0} d1={:.0} d2={:.0} t=({:.1},{:.1},{:.1})",
                self.pending.len(),
                first.x,
                first.y,
                first.z,
                matrix[0],
                matrix[5],
                matrix[10],
                matrix[12],
                matrix[13],
                matrix[14],
            )
        });

        if !self.staging.contains_key(&body_ptr) {
            self.staging.insert(
                body_ptr,
                BodyGrid {
                    min,
                    dims,
                    temps_c: Vec::new(),
                    version: 0,
                },
            );
        }
        let Some(grid) = self.staging.get_mut(&body_ptr) else {
            return;
        };
        grid.min = min;
        grid.dims = dims;
        if grid.temps_c.len() != total {
            grid.temps_c.clear();
            grid.temps_c.resize(total, f32::NAN);
        } else {
            grid.temps_c.fill(f32::NAN);
        }

        let t_air = air_temperature_at(origin);
        let t_ground = air_temperature_at((origin.0, 0.0, origin.2));
        if let Some(meta) = self.bodies.get_mut(&body_key) {
            meta.air_c = t_air;
            meta.ground_air_c = t_ground;
        }

        self.slots.clear();
        let mut hot_points: Vec<(Vec3, f32, HeatOwner)> = Vec::new();
        let mut changed = false;
        let dims_usize = [dims[0] as usize, dims[1] as usize, dims[2] as usize];

        let mut new_bound: HashMap<usize, (f32, u32, [i32; 3], Vec3, Vec3)> = HashMap::default();
        let mut conducted: HashMap<[i32; 3], (Vec3, f32)> = HashMap::default();
        for raw in &self.pending {
            let mut source_c = None;
            if let Some(&(t, pos)) = self.sources.get(&raw.component) {
                let w = voxel_to_world(&matrix, raw.x, raw.y, raw.z);
                let d = ((w.0 - pos.0).powi(2) + (w.1 - pos.1).powi(2) + (w.2 - pos.2).powi(2))
                    .sqrt();
                if d <= SOURCE_MAX_DIST_M {
                    source_c = Some(t);
                    // A hot source cell radiates to nearby cells of every body
                    // (hood over an engine), via the heat-point field.
                    if t > t_air + 5.0 {
                        hot_points.push((w, t, (body_ptr, [raw.x, raw.y, raw.z])));
                    }
                    if !self.bound.contains(&raw.component) {
                        new_bound
                            .entry(raw.component)
                            .or_insert((t, 0, [raw.x, raw.y, raw.z], w, pos))
                            .1 += 1;
                    }
                } else if !self.bound.contains(&raw.component) {
                    log_once(LOG_SRC_FAR, || {
                        format!(
                            "blocktemp: source component={:#x} temp={t:.1} C rejected: {d:.1} m from \
its voxel ({},{},{}) world=({:.1},{:.1},{:.1}) source_pos=({:.1},{:.1},{:.1})",
                            raw.component, raw.x, raw.y, raw.z, w.0, w.1, w.2, pos.0, pos.1, pos.2
                        )
                    });
                }
            }
            let x = raw.x - min[0];
            let y = raw.y - min[1];
            let z = raw.z - min[2];
            let index = grid_index([x as usize, y as usize, z as usize], dims_usize);
            if index >= total {
                continue;
            }
            let key = CellKey {
                vehicle_id,
                body_id,
                x: raw.x,
                y: raw.y,
                z: raw.z,
            };
            let capacity = (raw.mass_kg * SPECIFIC_HEAT_J_KG_K).max(MIN_CAPACITY_J_K);
            let temperature = match self.cells.entry(key) {
                Entry::Occupied(mut entry) => {
                    let state = entry.get_mut();
                    state.capacity_j_k = capacity;
                    state.last_seen_ms = now_ms;
                    state.temp_c
                }
                Entry::Vacant(entry) => {
                    entry.insert(CellState {
                        temp_c: t_air,
                        capacity_j_k: capacity,
                        t_env_c: t_air,
                        last_seen_ms: now_ms,
                    });
                    changed = true;
                    t_air
                }
            };
            grid.temps_c[index] = temperature;
            self.slots.push(CellSlot {
                index,
                x: raw.x,
                y: raw.y,
                z: raw.z,
                capacity_j_k: capacity,
                key,
                source_c,
                component: raw.component,
            });
        }
        if hot_points.is_empty() {
            self.heat_src.remove(&body_ptr);
        } else {
            self.heat_src.insert(body_ptr, hot_points);
        }
        crate::flir::set_body_plumes(body_ptr, jet_plumes(&self.pending, t_air));
        for (component, (t, cells, voxel, world, source_pos)) in new_bound {
            self.bound.insert(component);
            #[cfg(feature = "logging")]
            crate::logging::log_line(&format!(
                "blocktemp: source bound component={component:#x} temp={t:.1} C cells={cells} \
voxel={voxel:?} world=({:.1},{:.1},{:.1}) source_pos=({:.1},{:.1},{:.1})",
                world.0, world.1, world.2, source_pos.0, source_pos.1, source_pos.2
            ));
            #[cfg(not(feature = "logging"))]
            let _ = (t, cells, voxel, world, source_pos);
        }

        crate::fault::set(stage::BLOCKTEMP_PROBE);
        if !self.slots.is_empty() {
            self.scratch.clear();
            self.scratch.extend_from_slice(&grid.temps_c);
            // Per-body reject: a body nowhere near any heat point skips the
            // 27-bucket lookup for every one of its cells.
            let near_heat = self.fire.len > 0 && {
                let (mut lo, mut hi) = ((f64::MAX, f64::MAX, f64::MAX), (f64::MIN, f64::MIN, f64::MIN));
                for c in 0..8 {
                    let v = [
                        if c & 1 != 0 { min[0] + dims[0] as i32 - 1 } else { min[0] },
                        if c & 2 != 0 { min[1] + dims[1] as i32 - 1 } else { min[1] },
                        if c & 4 != 0 { min[2] + dims[2] as i32 - 1 } else { min[2] },
                    ];
                    let w = voxel_to_world(&matrix, v[0], v[1], v[2]);
                    lo = (lo.0.min(w.0), lo.1.min(w.1), lo.2.min(w.2));
                    hi = (hi.0.max(w.0), hi.1.max(w.1), hi.2.max(w.2));
                }
                // Half a voxel of slack for cell extents.
                let e = CELL_M as f64;
                self.fire.near_box((lo.0 - e, lo.1 - e, lo.2 - e), (hi.0 + e, hi.1 + e, hi.2 + e))
            };
            let h_env_w_k = H_ENV_W_M2_K * FACE_AREA_M2;
            let h_air_w_k = H_AIR_W_M2_K * FACE_AREA_M2;
            let solar_w = SOLAR_FLUX_W_M2 * FACE_AREA_M2 * sun;
            let seekers: Vec<usize> = lock(&SEEKERS).iter().map(|e| e.0).collect();
            for slot in &self.slots {
                let Some(&t) = self.scratch.get(slot.index) else {
                    continue;
                };
                if !t.is_finite() {
                    continue;
                }
                let local = [slot.x - min[0], slot.y - min[1], slot.z - min[2]];
                let mut neighbour_count = 0u32;
                let mut neighbour_sum = 0.0f32;
                for offset in NEIGHBOURS {
                    let nx = local[0] + offset[0];
                    let ny = local[1] + offset[1];
                    let nz = local[2] + offset[2];
                    if nx < 0
                        || ny < 0
                        || nz < 0
                        || nx >= dims[0] as i32
                        || ny >= dims[1] as i32
                        || nz >= dims[2] as i32
                    {
                        continue;
                    }
                    let neighbour_index =
                        grid_index([nx as usize, ny as usize, nz as usize], dims_usize);
                    if let Some(&neighbour) = self.scratch.get(neighbour_index) {
                        if neighbour.is_finite() {
                            neighbour_count += 1;
                            neighbour_sum += neighbour;
                        }
                    }
                }

                let t_env = if !near_heat {
                    t_air
                } else {
                    let world = voxel_to_world(&matrix, slot.x, slot.y, slot.z);
                    self.fire.env_temp(world, t_air, Some((body_ptr, [slot.x, slot.y, slot.z])))
                };
                if let Some(state) = self.cells.get_mut(&slot.key) {
                    state.t_env_c = t_env;
                }
                let input = StepInputs {
                    capacity_j_k: slot.capacity_j_k,
                    dt_s: dt,
                    t_env_c: t_env,
                    t_air_c: t_air,
                    // Only exposed faces see the fire/heat field, and only when a heat point is
                    // near (t_env > t_air); otherwise plain air convection (h_air) applies, so a
                    // climbing jet does not dump its heat to cold air at fire-coupling rates.
                    h_env_w_k: if t_env > t_air {
                        h_env_w_k * 6u32.saturating_sub(neighbour_count) as f32
                    } else {
                        0.0
                    },
                    h_air_w_k,
                    k_cond_w_k: K_COND_W_PER_K,
                    neighbour_count,
                    neighbour_sum_c: neighbour_sum,
                    // Only exposed faces take sun, about half of them facing it;
                    // a fully buried cell (6 neighbours) gets none.
                    solar_w: solar_w * 0.5 * 6u32.saturating_sub(neighbour_count) as f32 / 6.0,
                    source_c: slot.source_c,
                };
                let next = step_temperature(t, &input);
                if (next - t).abs() > TEMP_CHANGE_EPS_C {
                    changed = true;
                }
                grid.temps_c[slot.index] = next;
                if let Some(state) = self.cells.get_mut(&slot.key) {
                    state.temp_c = next;
                }
                if !seekers.is_empty() && seekers.contains(&slot.component) {
                    note_seeker_vehicle(slot.component, body_key.vehicle_id);
                }
                // Conduction-heated cells (a wing next to a jet engine) warm
                // parts of OTHER bodies touching them (landing gear on hinges).
                // Only surface cells can touch another body; 0.5 m cells
                // keep the field small (a hot wing has thousands of cells).
                if slot.source_c.is_none()
                    && neighbour_count < 6
                    && next > t_air + CONDUCTED_POINT_EXCESS_C
                {
                    let key = [slot.x.div_euclid(2), slot.y.div_euclid(2), slot.z.div_euclid(2)];
                    let full = conducted.len() >= MAX_CONDUCTED_POINTS;
                    match conducted.entry(key) {
                        Entry::Occupied(mut e) => {
                            if next > e.get().1 {
                                e.insert((voxel_to_world(&matrix, slot.x, slot.y, slot.z), next));
                            }
                        }
                        Entry::Vacant(e) if !full => {
                            e.insert((voxel_to_world(&matrix, slot.x, slot.y, slot.z), next));
                        }
                        Entry::Vacant(_) => {}
                    }
                }
            }
            let half = |i: usize| 0.5 * CELL_M as f64 * dims[i] as f64;
            let mid = |i: usize| min[i] + (dims[i] / 2) as i32;
            publish_body_ir(
                body_ptr,
                BodyIr {
                    stamp_ms: now_ms,
                    stamp_tick: SCENE_TICKS.load(std::sync::atomic::Ordering::Relaxed),
                    vehicle_id: body_key.vehicle_id,
                    center: voxel_to_world(&matrix, mid(0), mid(1), mid(2)),
                    vel: (0.0, 0.0, 0.0),
                    sphere_r: (half(0).powi(2) + half(1).powi(2) + half(2).powi(2)).sqrt(),
                },
            );
        }
        if !conducted.is_empty() {
            self.heat_src
                .entry(body_ptr)
                .or_default()
                .extend(conducted.into_values().map(|(w, t)| (w, t, (body_ptr, [BODY_WIDE_OWNER, 0, 0]))));
        }

        #[cfg(feature = "logging")]
        self.log_hottest(body_key, body_ptr, &matrix, now_ms);

        // Monotonic per-body version: the renderer's upload trigger. Only a
        // changed grid gets a new version, so an unchanged body is not uploaded
        // again every time it is stepped.
        if changed {
            let version = self.next_version;
            self.next_version = self.next_version.wrapping_add(1);
            if let Some(grid) = self.staging.get_mut(&body_ptr) {
                grid.version = version;
            }
        }

        self.seen.insert(body_ptr);
        self.stepped.insert(body_ptr);
    }

    /// Top-3 hottest cells of a body, at most every `HOT_LOG_INTERVAL_MS`.
    #[cfg(feature = "logging")]
    fn log_hottest(&mut self, key: BodyKey, body_ptr: usize, matrix: &[f64; 16], now_ms: u64) {
        let Some(meta) = self.bodies.get_mut(&key) else {
            return;
        };
        if now_ms.saturating_sub(meta.last_hot_log_ms) < HOT_LOG_INTERVAL_MS {
            return;
        }
        meta.last_hot_log_ms = now_ms;
        let mut top: Vec<(f32, &CellSlot)> = self
            .slots
            .iter()
            .filter_map(|s| self.cells.get(&s.key).map(|c| (c.temp_c, s)))
            .filter(|(t, _)| t.is_finite())
            .collect();
        top.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut line = format!(
            "blocktemp: hottest body={body_ptr:#x} vid={} bid={} cells={}:",
            key.vehicle_id,
            key.body_id,
            self.slots.len()
        );
        for (t, s) in top.iter().take(3) {
            let w = voxel_to_world(matrix, s.x, s.y, s.z);
            let env = self.cells.get(&s.key).map_or(f32::NAN, |c| c.t_env_c);
            line.push_str(&format!(
                " [voxel=({},{},{}) world=({:.1},{:.1},{:.1}) t={t:.1} C t_env={env:.1} src={} comp={:#x}]",
                s.x, s.y, s.z, w.0, w.1, w.2,
                s.source_c.map_or("none".to_string(), |v| format!("{v:.1}")),
                s.component
            ));
        }
        crate::logging::log_line(&line);
    }

    /// Copy this tick's stepped grids into [`PUBLISHED`] and drop the bodies
    /// that are gone.
    ///
    /// A body that is not due is not stepped, so it is not copied either: the
    /// published map keeps exactly the grid it last published (swapping whole
    /// maps instead published the stale half of the alternating staging grid
    /// and made the image flicker).
    fn publish(&mut self, now_ms: u64) {
        {
            let mut published = lock(&PUBLISHED);
            let published = published.get_or_insert_with(HashMap::default);
            published.retain(|ptr, _| self.seen.contains(ptr));
            for ptr in &self.stepped {
                let Some(grid) = self.staging.get(ptr) else {
                    continue;
                };
                match published.get_mut(ptr) {
                    // Reuse the published grid's allocation: the render thread
                    // only ever sees complete grids either way.
                    Some(slot) => {
                        slot.min = grid.min;
                        slot.dims = grid.dims;
                        slot.version = grid.version;
                        slot.temps_c.clear();
                        slot.temps_c.extend_from_slice(&grid.temps_c);
                    }
                    None => {
                        published.insert(*ptr, grid.clone());
                    }
                }
            }
        }
        // Staging keeps the body's grid for the next interlaced update; bodies
        // that are gone release their memory here.
        self.staging.retain(|ptr, _| self.seen.contains(ptr));

        if now_ms.saturating_sub(self.last_prune_ms) >= PRUNE_INTERVAL_MS {
            self.last_prune_ms = now_ms;
            self.cells
                .retain(|_, state| now_ms.saturating_sub(state.last_seen_ms) < STALE_BODY_MS);
            self.bodies
                .retain(|_, meta| now_ms.saturating_sub(meta.last_seen_ms) < STALE_BODY_MS);
        }
    }
}

// ---------------------------------------------------------------------------
// Self-checks (pure step only; the game-facing walk cannot run off-target)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn isolated(
        capacity_j_k: f32,
        dt_s: f32,
        t_env_c: f32,
        t_air_c: f32,
        solar_w: f32,
    ) -> StepInputs {
        StepInputs {
            capacity_j_k,
            dt_s,
            t_env_c,
            t_air_c,
            h_env_w_k: H_ENV_W_M2_K * FACE_AREA_M2,
            h_air_w_k: H_AIR_W_M2_K * FACE_AREA_M2,
            k_cond_w_k: K_COND_W_PER_K,
            neighbour_count: 0,
            neighbour_sum_c: 0.0,
            solar_w,
            source_c: None,
        }
    }

    fn conduction_only(capacity_j_k: f32, dt_s: f32, neighbour: f32) -> StepInputs {
        StepInputs {
            capacity_j_k,
            dt_s,
            t_env_c: 0.0,
            t_air_c: 0.0,
            h_env_w_k: 0.0,
            h_air_w_k: 0.0,
            k_cond_w_k: K_COND_W_PER_K,
            neighbour_count: 1,
            neighbour_sum_c: neighbour,
            solar_w: 0.0,
            source_c: None,
        }
    }

    #[test]
    fn conduction_conserves_energy_without_environment() {
        let capacity = 500.0f32; // same on both cells -> symmetric pair
        let dt = 0.5f32;
        let mut a = 100.0f32;
        let mut b = 0.0f32;
        let initial_energy = capacity * (a + b);
        // 5000 steps of 0.5 s is ~15 difference time constants (C/(2k) = 167 s),
        // so the pair should be fully equalised at the end.
        for _ in 0..5000 {
            // Both cells see the OTHER cell's old temperature: the pair's
            // energy change cancels exactly, so conduction only redistributes.
            let next_a = step_temperature(a, &conduction_only(capacity, dt, b));
            let next_b = step_temperature(b, &conduction_only(capacity, dt, a));
            a = next_a;
            b = next_b;
        }
        let final_energy = capacity * (a + b);
        assert!(
            (final_energy - initial_energy).abs() < 0.1,
            "conduction changed total energy: {initial_energy} -> {final_energy}"
        );
        assert!(a < 100.0 && b > 0.0, "heat did not flow: a={a} b={b}");
        assert!((a - b).abs() < 1.0, "cells did not equalise: a={a} b={b}");
    }

    #[test]
    fn heavier_cell_heats_slower() {
        let light = isolated(500.0, 1.0 / 60.0, 200.0, 20.0, 0.0);
        let heavy = isolated(5000.0, 1.0 / 60.0, 200.0, 20.0, 0.0);
        let t0 = 20.0f32;
        let light_next = step_temperature(t0, &light);
        let heavy_next = step_temperature(t0, &heavy);
        let light_rise = light_next - t0;
        let heavy_rise = heavy_next - t0;
        assert!(light_rise > 0.0 && heavy_rise > 0.0);
        assert!(
            heavy_rise < light_rise,
            "heavy cell rose faster: light={light_rise} heavy={heavy_rise}"
        );
        assert!(
            (light_rise / heavy_rise - 10.0).abs() < 0.1,
            "heat rise should scale inversely with capacity: {light_rise} vs {heavy_rise}"
        );
    }

    #[test]
    fn step_is_stable_at_extreme_dt_and_capacity() {
        let input = StepInputs {
            capacity_j_k: 1.0,
            dt_s: 1000.0,
            t_env_c: 900.0,
            t_air_c: 20.0,
            h_env_w_k: H_ENV_W_M2_K * FACE_AREA_M2,
            h_air_w_k: H_AIR_W_M2_K * FACE_AREA_M2,
            k_cond_w_k: K_COND_W_PER_K,
            neighbour_count: 6,
            neighbour_sum_c: 6.0 * 20.0,
            solar_w: 25.0,
            source_c: None,
        };
        let next = step_temperature(20.0, &input);
        assert!(next.is_finite());
        assert!(
            (20.0..=900.0).contains(&next),
            "step overshot its target: {next}"
        );
    }

    #[test]
    fn non_finite_state_recovers_to_air() {
        let input = isolated(500.0, 1.0 / 60.0, 100.0, 15.0, 0.0);
        assert!((step_temperature(f32::NAN, &input) - 15.0).abs() < f32::EPSILON);
        let mut bad_capacity = input;
        bad_capacity.capacity_j_k = f32::NAN;
        assert!((step_temperature(20.0, &bad_capacity) - 15.0).abs() < f32::EPSILON);
    }

    #[test]
    fn source_cell_reaches_its_temp_and_warms_neighbour() {
        let capacity = 5000.0f32;
        let dt = 1.0 / 15.0;
        let (mut hot, mut next_door) = (20.0f32, 20.0f32);
        for _ in 0..(15 * 10) {
            let mut a = isolated(capacity, dt, 20.0, 20.0, 0.0);
            a.neighbour_count = 1;
            a.neighbour_sum_c = next_door;
            a.source_c = Some(70.0);
            let mut b = isolated(capacity, dt, 20.0, 20.0, 0.0);
            b.neighbour_count = 1;
            b.neighbour_sum_c = hot;
            let (na, nb) = (step_temperature(hot, &a), step_temperature(next_door, &b));
            hot = na;
            next_door = nb;
        }
        assert!((hot - 70.0).abs() < 2.0, "source cell at {hot}");
        assert!(next_door > 20.05 && next_door < hot, "neighbour at {next_door}");
    }

    #[test]
    fn bounds_reject_oversized_and_empty_grids() {
        assert!(grid_bounds(&[]).is_none());
        let one = [RawCell {
            x: 0,
            y: 0,
            z: 0,
            mass_kg: 1.0,
            component: 0,
        }];
        let (min, dims, total) = grid_bounds(&one).expect("single cell fits");
        assert_eq!(min, [0, 0, 0]);
        assert_eq!(dims, [1, 1, 1]);
        assert_eq!(total, 1);

        let mut huge = Vec::new();
        huge.push(RawCell {
            x: 0,
            y: 0,
            z: 0,
            mass_kg: 1.0,
            component: 0,
        });
        huge.push(RawCell {
            x: MAX_GRID_DIM as i32,
            y: 0,
            z: 0,
            mass_kg: 1.0,
            component: 0,
        });
        assert!(grid_bounds(&huge).is_none(), "axis cap not enforced");
    }

    #[test]
    fn publish_keeps_the_last_stepped_grid_for_a_body_that_is_not_due() {
        // Body 0x1000 is stepped with 40 C, then a tick arrives on which it is
        // not due (only `seen`). The staging grid still holds the previous
        // build, but publishing it would flip the image back and forth.
        let body = 0x1000usize;
        let grid = || BodyGrid {
            min: [0, 0, 0],
            dims: [1, 1, 1],
            temps_c: vec![40.0],
            version: 7,
        };
        let mut sim = Sim::default();
        sim.staging.insert(body, grid());
        sim.seen.insert(body);
        sim.stepped.insert(body);
        sim.publish(0);

        sim.staging.get_mut(&body).unwrap().temps_c[0] = 99.0;
        sim.stepped.clear();
        sim.publish(0);

        {
            let published = lock(&PUBLISHED);
            let entry = published.as_ref().unwrap().get(&body).unwrap();
            assert_eq!(entry.temps_c, vec![40.0], "published grid reverted");
            assert_eq!(entry.version, 7);
            // A body that is gone is dropped from both maps.
        }
        sim.seen.clear();
        sim.publish(0);
        let published = lock(&PUBLISHED);
        assert!(published.as_ref().unwrap().get(&body).is_none());
        assert!(sim.staging.get(&body).is_none());
    }

    #[test]
    fn grid_index_is_x_fastest_matching_texture_layout() {
        let dims = [3usize, 4, 5];
        assert_eq!(grid_index([0, 0, 0], dims), 0);
        assert_eq!(grid_index([1, 0, 0], dims), 1);
        assert_eq!(grid_index([0, 1, 0], dims), 3);
        assert_eq!(grid_index([0, 0, 1], dims), 12);
        assert_eq!(grid_index([2, 3, 4], dims), 59);
    }

    #[test]
    fn fire_falloff_and_hash_lookup() {
        // Falloff: full inside 0.4 m, linear to air at 1.0 m.
        assert_eq!(fire_temp_at(0.0, 20.0), FIRE_TEMP_C);
        assert_eq!(fire_temp_at(0.4, 20.0), FIRE_TEMP_C);
        let mid = fire_temp_at(0.7, 20.0);
        assert!((mid - 210.0).abs() < 0.5, "mid={mid}");
        assert_eq!(fire_temp_at(1.0, 20.0), 20.0);
        assert_eq!(fire_temp_at(5.0, 20.0), 20.0);
        // Never cools below a hotter air temperature.
        assert_eq!(fire_temp_at(2.0, 500.0), 500.0);

        let mut field = FireField::default();
        assert_eq!(field.env_temp((0.0, 0.0, 0.0), 20.0, None), 20.0);
        // Points straddling a bucket border and in a negative bucket.
        field.insert((0.99, 0.0, -0.01), FIRE_TEMP_C, None);
        field.insert((-10.2, 3.0, 7.7), FIRE_TEMP_C, None);
        assert_eq!(field.len, 2);
        // Neighbour bucket (x = 1.2) is within 0.4 m of the first point.
        assert_eq!(field.env_temp((1.2, 0.0, 0.0), 20.0, None), FIRE_TEMP_C);
        assert_eq!(field.env_temp((-10.2, 3.2, 7.7), 20.0, None), FIRE_TEMP_C);
        let t = field.env_temp((0.99 + 0.7, 0.0, -0.01), 20.0, None);
        assert!((t - 210.0).abs() < 0.5, "t={t}");
        // Far from every voxel: air.
        assert_eq!(field.env_temp((3.0, 0.0, 0.0), 20.0, None), 20.0);
        assert_eq!(field.env_temp((0.0, 50.0, 0.0), 20.0, None), 20.0);
        // A 100 C point: lerps 100 -> air between 0.4 m and 1.0 m.
        let mut hot = FireField::default();
        hot.insert((0.0, 0.0, 0.0), 100.0, None);
        let t = hot.env_temp((0.7, 0.0, 0.0), 20.0, None);
        assert!((t - 60.0).abs() < 0.5, "t={t}");
    }

    #[test]
    fn field_near_box_rejects_far_bodies() {
        let mut field = FireField::default();
        assert!(!field.near_box((0.0, 0.0, 0.0), (1.0, 1.0, 1.0)));
        field.insert((10.0, 0.0, 0.0), 400.0, None);
        field.insert((12.0, 2.0, 0.0), 400.0, None);
        assert!(field.near_box((8.5, 0.0, 0.0), (9.5, 1.0, 1.0)));
        assert!(field.near_box((11.0, 1.0, -0.5), (11.5, 1.5, 0.5)));
        assert!(!field.near_box((0.0, 0.0, 0.0), (8.9, 1.0, 1.0)));
        assert!(!field.near_box((10.0, 0.0, 1.5), (11.0, 1.0, 3.0)));
    }

    #[test]
    fn source_cell_excludes_itself_but_warms_others() {
        let mut field = FireField::default();
        field.insert((0.0, 0.0, 0.0), 300.0, Some((1, [0, 0, 0])));
        // Own point ignored; another body's cell at the same spot sees it.
        assert_eq!(field.env_temp((0.0, 0.0, 0.0), 20.0, Some((1, [0, 0, 0]))), 20.0);
        assert_eq!(field.env_temp((0.0, 0.0, 0.0), 20.0, Some((2, [0, 0, 0]))), 300.0);
        assert_eq!(field.env_temp((0.25, 0.0, 0.0), 20.0, Some((1, [1, 0, 0]))), 300.0);
    }

    #[test]
    fn principal_axis_of_a_long_box() {
        let pts = (0..3).flat_map(|x| (0..3).flat_map(move |y| (0..6).map(move |z| [x as f32, y as f32, z as f32])));
        let (a, spread) = principal_axis(pts);
        assert!(a[2].abs() > 0.99 && spread > 1.5, "{a:?} {spread}");
    }

    #[test]
    fn conducted_point_skips_its_own_body() {
        let mut field = FireField::default();
        field.insert((0.0, 0.0, 0.0), 200.0, Some((1, [BODY_WIDE_OWNER, 0, 0])));
        assert_eq!(field.env_temp((0.0, 0.0, 0.0), 20.0, Some((1, [3, 0, 0]))), 20.0);
        assert_eq!(field.env_temp((0.0, 0.0, 0.0), 20.0, Some((2, [0, 0, 0]))), 200.0);
    }
}
