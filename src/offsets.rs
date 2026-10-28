//! Reverse-engineered game addresses, byte-signature gates and tuning
//! constants. Everything here is a fact about `stormworks64.exe`; keep it in
//! one place so a game update is a single-file diff.

/// Build this DLL was relocated against.
/// stormworks64.exe sha256:
/// f7d605e34fe3b8ebde3adbc8e2e90fe28821c6cb3ef67be79abe58ce652968a0 (v1.15.25; offsets mapped from f9206d85c82f, see stormworks_idapro/new/UPDATE_MAP.md)
pub(crate) const GAME_BUILD_LABEL: &str = "f7d605e34fe3";

/// Unique stamp of THIS plugin binary (unix seconds at compile time), logged
/// at bootstrap. On-disk checksums cannot tell you what a running game has
/// loaded; this can - the stamp in the log must match the stamp of the build
/// you deployed.
pub(crate) const PLUGIN_BUILD_LABEL: &str = env!("INFRARED_BUILD_STAMP");

// RVAs (image base 0x140000000).
pub(crate) const RVA_COMPONENT_LUA_REGISTRAR: usize = 0x0032_e3d0; // FUN_14032be50
// The screen/microcontroller Lua initializer: it builds the `debug`, `async` and
// `screen` tables, so it is the state an on-screen image has to be drawn from.
// The component-mod registrar above only reaches component scripts, which cannot
// draw, and its state has no screen API.
pub(crate) const RVA_LUA_SCREEN_INIT: usize = 0x002e_7740; // FUN_1402e5230
pub(crate) const RVA_TEMPERATURE: usize = 0x0031_22b0; // FUN_14030fd60
pub(crate) const RVA_ARG_SLOT: usize = 0x0009_7910; // FUN_140097920
pub(crate) const RVA_PUSH_STRING: usize = 0x0009_7f50; // FUN_140097f60
pub(crate) const RVA_CREATE_TABLE: usize = 0x000b_a820; // FUN_1400ba830
pub(crate) const RVA_RAWSETI: usize = 0x0009_8560; // FUN_140098570
pub(crate) const RVA_REGISTER_TABLE: usize = 0x001e_6330; // FUN_1401e22d0
pub(crate) const RVA_STRING_ALLOC: usize = 0x0017_ca10; // FUN_14017cbb0
pub(crate) const RVA_STRING_FILL: usize = 0x009a_0cf0; // FUN_14099d3b0
pub(crate) const RVA_BASE_TEMP: usize = 0x0031_25f0; // FUN_1403100a0(pos, windField, weatherGrid, ignored, waterFlag) -> ambient
pub(crate) const RVA_RAYCAST: usize = 0x001a_4010; // FUN_1401a4160(ctx,seg6d,out_hit,out_obj,grp,mask)->bool
pub(crate) const RVA_FIRES_BUILD: usize = 0x0030_c800; // FUN_14030a2f0 fire vertex builder (holder owns the fires ring)
// scene_vehicle_update: once per sim tick on the update thread, rcx = c_scene (0x5E85E0 was load-only).
pub(crate) const RVA_SCENE_TICK: usize = 0x005e_bcf0;
pub(crate) const RVA_SETTINGS: usize = 0x0029_6940; // FUN_140294520() -> settings singleton ([0] = day length, minutes)
pub(crate) const RVA_FRAME_UNIFORMS: usize = 0x006b_e180; // FUN_1406bb400 render-thread uniform fill; arg2 = the scene object
pub(crate) const FRAME_UNIFORMS_PROLOGUE: [u8; 32] = [
    0x40, 0x55, 0x53, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48, 0x8d, 0xac,
    0x24, 0xc8, 0x77, 0xff, 0xff, 0xb8, 0x58, 0x89, 0x00, 0x00, 0xe8, 0x41, 0x86, 0x31, 0x00, 0x48,
];
// The game's exhaust-plume emitters: every engine type creates them through
// FUN_140306740(manager, out_handle, effect_index) and updates them each tick
// through FUN_140302bb0(emitter, world_pos, vel, spread..., rate, ...). The
// emitter points at its effect definition, which carries the index (+0x10) and
// the name (+0x00), so the single update hook classifies every plume itself.
// The propulsion components' per-tick update. These are the blocks themselves:
// the hook reads the engine's own state (burn/throttle/heat) and its world
// position, so the IR signature follows the engine rather than its particles.
//   rocket-family tick  FUN_140437570 (vtable slot 107 of the rocket class)
//     +0x778 power/burn fraction 0..1 (scales thrust by 600000)
//     +0x848/+0x84c/+0x850 thrust vector, +0x7b0/+0x7b8/+0x7c0 plume emitters
//   jet-family tick     FUN_140350f40 (vtable slot 107 of the jet class)
//     +0x1bc smoothed power (fed by fuel drain, decays 0.99/tick)
//     +0x1cc exhaust temperature (>1000 spawns fire), +0x1d8 heat 0..1
// Both take (component, ?, ?, ?, world_matrix, origin, ...): all four register
// arguments are integer-class (param_4 is stored from R9), so a detour can
// declare every argument usize with no float-register hazard.
pub(crate) const RVA_ROCKET_TICK: usize = 0x0043_9df0; // FUN_140437570
pub(crate) const ROCKET_TICK_PROLOGUE: [u8; 32] = [
    0x48, 0x8b, 0xc4, 0x48, 0x89, 0x58, 0x10, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56,
    0x41, 0x57, 0x48, 0x8d, 0xa8, 0x48, 0xfb, 0xff, 0xff, 0x48, 0x81, 0xec, 0x80, 0x05, 0x00, 0x00,
];
// The two diesel engine families have their own ticks, reached the same way
// (vtable slot 107): the preset block c_scene_vehicle_component_engine
// (vtable 0x140b6f4b0) and the modular engine
// c_scene_vehicle_component_modular_engine (vtable 0x140b9c8b0). Both take the
// component in RCX and compute their exhaust world position into the component
// at +0x390 as three doubles, so the detours read the position there instead of
// transforming the tick's matrix argument.
pub(crate) const RVA_PRESET_ENGINE_TICK: usize = 0x0034_df80; // FUN_14034ba00
/// Verified in game: the preset engine block keeps its temperature in Celsius
/// at +0x1d8 - it read 11.8 C cold and 43.7 C running at 0.133 throttle, which
/// matches the temperature the game itself displays for the block.
pub(crate) const PRESET_TEMP_OFFSET: usize = 0x1d8;
/// Modular engine cylinder temperature. Verified in game: +0x59c climbed
/// 19.97 -> 66.87 C as the engine warmed, matching the 66-67 C the game shows
/// for the cylinder. Only cylinder parts carry this field, so hot cylinders
/// emit without having to identify part names.
pub(crate) const MODULAR_TEMP_OFFSET: usize = 0x59c;
/// Surface temperature of the fluid/heat machinery (water class). Verified: a
/// 3x3 radiator read 32.399 C while its fluid inlet sat at 60.2 C, matching the
/// 32.4 C measured on the radiator itself.
pub(crate) const FLUID_TEMP_OFFSET: usize = 0x240;
/// Below this a block or radiator is still cold, so no IR source is published.
pub(crate) const ENGINE_MIN_TEMP_C: f64 = 25.0;
/// Kept as the preset engine's own name for the same threshold.
pub(crate) const PRESET_ENGINE_MIN_C: f64 = ENGINE_MIN_TEMP_C;

/// A seeker's identity is 24 bits: an f32 carries an integer exactly to 2^24, so
/// it survives the trip through a composite as a single number. It is not chosen
/// by hand - it is derived from the component - so two live seekers only collide
/// if their mixed pointers do (about 1 in 16.7 million per pair), and a collision
/// only misroutes a display image: the guidance numbers travel on the seeker's
/// own composite wire, not through this id.
pub(crate) const FRAME_ID_MAX: u32 = 0xff_ffff;
/// Cap on the id -> frame map. Ids come from component pointers and a long
/// session destroys and respawns vehicles, so the map is bounded rather than
/// left to grow; on overflow it is cleared, which costs one frame because every
/// live seeker republishes on its next scan.
pub(crate) const FRAME_MAP_MAX: usize = 64;
pub(crate) const PRESET_ENGINE_TICK_PROLOGUE: [u8; 32] = [
    0x48, 0x8b, 0xc4, 0x48, 0x89, 0x58, 0x10, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56,
    0x41, 0x57, 0x48, 0x8d, 0xa8, 0x48, 0xfe, 0xff, 0xff, 0x48, 0x81, 0xec, 0x80, 0x02, 0x00, 0x00,
];
pub(crate) const RVA_MODULAR_ENGINE_TICK: usize = 0x0044_bef0; // FUN_140449670
pub(crate) const MODULAR_ENGINE_TICK_PROLOGUE: [u8; 32] = [
    0x48, 0x8b, 0xc4, 0x48, 0x89, 0x58, 0x10, 0x48, 0x89, 0x70, 0x18, 0x55, 0x57, 0x41, 0x54, 0x41,
    0x56, 0x41, 0x57, 0x48, 0x8d, 0xa8, 0x58, 0xfe, 0xff, 0xff, 0x48, 0x81, 0xec, 0x80, 0x02, 0x00,
];
// Fluid/heat machinery (part type 24: radiators, heat exchangers, tanks,
// valves, pumps...). Type 24 spans several component classes, so the two
// heat-relevant ones are probed: the water class (0x140b08f18) and the steam
// class (0x140af6dc8). Both ticks take the component in RCX like the engine
// ticks.
pub(crate) const RVA_WATER_TICK: usize = 0x003f_1fa0; // FUN_1403ef860
pub(crate) const WATER_TICK_PROLOGUE: [u8; 32] = [
    0x48, 0x8b, 0xc4, 0x48, 0x89, 0x58, 0x10, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56,
    0x41, 0x57, 0x48, 0x8d, 0xa8, 0x98, 0xf9, 0xff, 0xff, 0x48, 0x81, 0xec, 0x30, 0x07, 0x00, 0x00,
];
pub(crate) const RVA_STEAM_TICK: usize = 0x0040_f3a0; // FUN_14040cbd0
pub(crate) const STEAM_TICK_PROLOGUE: [u8; 32] = [
    0x48, 0x8b, 0xc4, 0x48, 0x89, 0x58, 0x10, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56,
    0x41, 0x57, 0x48, 0x8d, 0xa8, 0xf8, 0xfb, 0xff, 0xff, 0x48, 0x81, 0xec, 0xd0, 0x04, 0x00, 0x00,
];
/// Diesel exhaust temperature range (provisional; the block runs far cooler
/// than a jet or rocket plume).
pub(crate) const DIESEL_T_MIN_C: f64 = 80.0;
pub(crate) const DIESEL_T_MAX_C: f64 = 520.0;

pub(crate) const RVA_JET_TICK: usize = 0x0035_34c0; // FUN_140350f40
pub(crate) const JET_TICK_PROLOGUE: [u8; 32] = [
    0x48, 0x8b, 0xc4, 0x48, 0x89, 0x58, 0x10, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56,
    0x41, 0x57, 0x48, 0x8d, 0xa8, 0x08, 0xfe, 0xff, 0xff, 0x48, 0x81, 0xec, 0xc0, 0x02, 0x00, 0x00,
];
// Engine block state -> IR source. Temperatures are provisional pending in-game
// calibration against the game's own numbers.
pub(crate) const ENGINE_MIN_POWER: f64 = 0.02;
/// Component definition fields holding the block's voxel bounds, parsed from
/// the part XML (voxel_min at +0xc0, voxel_max at +0xcc, 12 bytes each).
pub(crate) const DEF_VOXEL_MIN: usize = 0xc0;
pub(crate) const DEF_VOXEL_MAX: usize = 0xcc;
/// One voxel is 0.25 m - the same scale the component tick uses for position.
pub(crate) const VOXEL_M: f64 = 0.25;
/// Plume radius from the block size: half the block's middle extent, narrowed
/// at low throttle (a plume gets thinner, but not to nothing, as it throttles).
pub(crate) const ENGINE_R_BLOCK_FACTOR: f64 = 0.5;
pub(crate) const ENGINE_R_MIN_M: f64 = 0.12;
pub(crate) const ENGINE_R_MAX_M: f64 = 3.0;
/// Block size assumed when a definition's voxel bounds cannot be read.
pub(crate) const ENGINE_DEFAULT_SIZE_M: f64 = 0.75;
/// Activity used for a rocket that is burning fuel but reports no burn
/// fraction (solid boosters), so it still emits at a sensible temperature.
pub(crate) const ROCKET_DRAIN_ACTIVITY: f64 = 0.6;
pub(crate) const ROCKET_T_MIN_C: f64 = 200.0;
pub(crate) const ROCKET_T_MAX_C: f64 = 900.0;
/// Compressor section: compression heating only, roughly flat with spool.
pub(crate) const JET_COMPRESSOR_T_C: f64 = 200.0;
/// Combustion chamber and turbine radiate as cooled CASING, not as the gas
/// inside - that gas never reaches the outside world. The open exhaust does,
/// which is why it owns the strongest signature.
pub(crate) const JET_CASING_T_LOW: f64 = 300.0;
pub(crate) const JET_CASING_T_HIGH: f64 = 500.0;
/// Open exhaust nozzle: the hot gas leaving the engine.
pub(crate) const JET_EXHAUST_T_LOW: f64 = 500.0;
pub(crate) const JET_EXHAUST_T_HIGH: f64 = 900.0;
/// The same exhaust with the afterburner lit.
pub(crate) const JET_AB_T_LOW: f64 = 1400.0;
pub(crate) const JET_AB_T_HIGH: f64 = 1900.0;
/// The exhaust part's 0..1 flow field at +0x1d8 tracks the engine's operating
/// point (0.06 idling, ~1.0 at full spool) because the exhaust never
/// accumulates spool of its own - case 4 of the tick does not touch +0x1bc.
pub(crate) const JET_EXHAUST_FLOW_FULL: f64 = 1.0;
/// Afterburner toggle on the exhaust part: in case 4 of the jet tick this bool
/// gates a thrust multiplier of (fuel/0.001)*0.5 + 1, up to 1.5x - the
/// "moderate increase in thrust" of a lit afterburner.
pub(crate) const JET_AFTERBURNER_OFFSET: usize = 0x240;
/// jet_engine_component_type of an exhaust nozzle.
pub(crate) const JET_EXHAUST_PART_TYPE: u32 = 4;
/// Glare (sensor bloom). A core bright enough to clip at the gain stage
/// scatters a fraction of that light across the array, so a saturating source
/// dominates the frame instead of clipping into one bright dot. The halo peaks
/// at BLOOM_PEAK (of full scale) however bright the source, and falls off as a
/// gaussian over BLOOM_RADIUS_CELLS.
pub(crate) const BLOOM_PEAK: f64 = 0.6;
pub(crate) const BLOOM_RADIUS_CELLS: f64 = 3.0;
/// Jet part type, read from the part definition. The jet tick switches on
/// *(definition + 0xed4): 0/1 = intake/compressor stage, 2 = combustion
/// chamber (adds fuel * 1600 to the part temperature), 3 = turbine (extracts
/// power from the part temperature and halves it), 4 = exhaust nozzle (spawns
/// the plume once the part temperature passes 1000 C).
pub(crate) const DEF_JET_PART_TYPE: usize = 0xed4;
/// Jet spool speed, in RPS. Case 3 (the turbine) does
/// `+0x1bc += (+0x1cc * 0.5) * 7.5e-05`, so every turbine adds to it, and case 2
/// treats 200.0 as an over-speed event - this single field therefore already
/// carries both "how many turbines" and "how fast", the two things a jet's
/// signature is supposed to scale with.
pub(crate) const JET_RPS_OFFSET: usize = 0x1bc;
/// The spool speed jets are actually flown at - around 10 RPS to save fuel,
/// with parallel turbines making up the thrust. The per-role temperatures are
/// anchored here and rise to their full-spool values at JET_RPS_FULL.
pub(crate) const JET_RPS_TYPICAL: f64 = 10.0;
/// Spool speed treated as full power. Jets destroy themselves somewhere past
/// 100 RPS, so 100 is the natural "full" reference. Single tunable.
pub(crate) const JET_RPS_FULL: f64 = 100.0;
pub(crate) const RVA_EXHAUST_UPDATE: usize = 0x0030_50c0; // FUN_140302bb0(emitter, world_pos, ..., rate, ...)
pub(crate) const EXHAUST_UPDATE_PROLOGUE: [u8; 32] = [
    0x48, 0x8b, 0xc4, 0x48, 0x89, 0x58, 0x18, 0x4c, 0x89, 0x48, 0x20, 0x55, 0x56, 0x57, 0x41, 0x54,
    0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48, 0x81, 0xec, 0xe0, 0x00, 0x00, 0x00, 0x0f, 0x29, 0x70,
];
// Effect indices of the exhaust/rocket plumes (see the definition table built
// by FUN_1403054f0): 2 exhaust, 3 exhaust_damage_1 (the player-killing field),
// 4 exhaust_strip, 5 exhaust_damage_1_strip, 6 exhaust_splash,
// 7 exhaust_damage_1_splash, 41 rocket, 42 rocket_strip.
pub(crate) const EMITTER_MAX_PENDING: usize = 192;
pub(crate) const EMITTER_MIN_RATE: f64 = 0.002;
// Exhaust temperatures by effect family (degrees C, provisional).
pub(crate) const EXHAUST_T_PLUME: f64 = 500.0; // 2/4/6
pub(crate) const EXHAUST_T_DAMAGE: f64 = 800.0; // 3/5/7 - the kill field
pub(crate) const EXHAUST_T_ROCKET: f64 = 900.0; // 41/42
pub(crate) const RVA_SUNROT: usize = 0x002c_1ef0; // FUN_1402bf9e0(day_percent_f32, dir3_out, mat_out) — the game's rotating sun
pub(crate) const RVA_SUN_UPDATE: usize = 0x005e_6030; // FUN_1405e3270(ctx, day_percent, flags, out) — per-tick sun update; out+0x198 = sun sprite world position
pub(crate) const SUN_UPDATE_PROLOGUE: [u8; 32] = [
    0x48, 0x8b, 0xc4, 0x55, 0x53, 0x56, 0x57, 0x41, 0x54, 0x41, 0x56, 0x41, 0x57, 0x48, 0x8d, 0x6c,
    0x24, 0xb0, 0x48, 0x81, 0xec, 0x50, 0x01, 0x00, 0x00, 0x0f, 0x29, 0x70, 0xb8, 0x0f, 0x29, 0x78,
];

// Occlusion tuning: ignore blockers within OCCLUDE_MIN_DIST of the sensor (its
// own airframe) and within OCCLUDE_END_SLACK of the emitter (its own hull edge).
pub(crate) const OCCLUDE_MIN_DIST: f64 = 1.0;
pub(crate) const OCCLUDE_END_SLACK: f64 = 2.0;

pub(crate) const FIRST_UPVALUE_INDEX: i32 = -1_001_001; // 0xfff0b9d7
pub(crate) const COMPONENT_MATRIX_OFFSET: usize = 0x1e8;

pub(crate) const REGISTRAR_PROLOGUE: [u8; 32] = [
    0x48, 0x89, 0x5c, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x55, 0x57, 0x41, 0x56, 0x48, 0x8d,
    0xac, 0x24, 0x50, 0xfc, 0xff, 0xff, 0x48, 0x81, 0xec, 0xb0, 0x04, 0x00, 0x00, 0x48, 0x8b, 0x05,
];
/// Same shape as the component-mod registrar, with a smaller stack frame (0x320).
pub(crate) const SCREEN_INIT_PROLOGUE: [u8; 32] = [
    0x48, 0x89, 0x5c, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x55, 0x57, 0x41, 0x56, 0x48, 0x8d,
    0xac, 0x24, 0xe0, 0xfd, 0xff, 0xff, 0x48, 0x81, 0xec, 0x20, 0x03, 0x00, 0x00, 0x48, 0x8b, 0x05,
];
pub(crate) const TEMPERATURE_PROLOGUE: [u8; 32] = [
    0x48, 0x8b, 0xc4, 0x48, 0x89, 0x58, 0x10, 0x48, 0x89, 0x68, 0x18, 0x48, 0x89, 0x70, 0x20, 0x57,
    0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48, 0x81, 0xec, 0xb0, 0x00, 0x00, 0x00, 0x0f,
];
pub(crate) const FIRES_PROLOGUE: [u8; 32] = [
    0x48, 0x8b, 0xc4, 0x48, 0x89, 0x58, 0x08, 0x48, 0x89, 0x70, 0x18, 0x48, 0x89, 0x78, 0x20, 0x55,
    0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48, 0x8d, 0x6c, 0x24, 0xa0, 0x48, 0x81, 0xec,
];
pub(crate) const SETTINGS_PROLOGUE: [u8; 32] = [
    0x48, 0x83, 0xec, 0x28, 0xba, 0x04, 0x00, 0x00, 0x00, 0x65, 0x48, 0x8b, 0x04, 0x25, 0x58, 0x00,
    0x00, 0x00, 0x48, 0x8b, 0x08, 0x8b, 0x04, 0x0a, 0x39, 0x05, 0xde, 0x96, 0xa6, 0x00, 0x7e, 0x33,
];
pub(crate) const SCENE_TICK_PROLOGUE: [u8; 32] = [
    0x48, 0x8b, 0xc4, 0x55, 0x53, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48,
    0x8d, 0xa8, 0x48, 0xfd, 0xff, 0xff, 0x48, 0x81, 0xec, 0xa8, 0x03, 0x00, 0x00, 0x0f, 0x29, 0x70,
];

// ---------------------------------------------------------------------------
// FLIR render side (docs/FLIR_DESIGN.md "Render pipeline")
// ---------------------------------------------------------------------------

/// Tonemap pass: `fastcall(mat_set, view, params* a3)` — 3 integer-class
/// arguments, void return (verified by disassembly). `a3+0x00` is `color_mode`,
/// non-zero for a view that has IR enabled.
pub(crate) const RVA_TONEMAP: usize = 0x0068_e640; // FUN_14068b8c0
pub(crate) const TONEMAP_PROLOGUE: [u8; 24] = [
    0x48, 0x89, 0x5c, 0x24, 0x08, 0x48, 0x89, 0x6c, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x57,
    0x41, 0x56, 0x41, 0x57, 0x48, 0x83, 0xec, 0x50,
];
/// Tonemap uniforms `0x68B8C0` `r8` = `view_state + TONEMAP_PARAMS_OFF`.
pub(crate) const VIEW_STATE_TONEMAP_PARAMS: usize = 0x870;
/// View state field holding the per-monitor render-target holder W (the ring
/// entry `+0x148` value; null for the main view). Persistent, unlike the feed
/// view state which is freed every frame.
pub(crate) const VIEW_STATE_RT_HOLDER: usize = 0x828;
/// `c_scene_vehicle_component_camera` vtable (RVA); validates a camera pointer.
pub(crate) const RVA_CAMERA_VTABLE: usize = 0x00b5_d5d0;
/// `0x68BB00 draw_fullscreen_quad_tex5`: its single `glDrawElements` (at
/// `0x68BC6A`, through the IAT slot `0x140A935F8`) returns into
/// `[0x68BB00, 0x68BCA0)`; the next function starts at `0x68BCA0`. That draw
/// is the tonemap quad, i.e. where the IR composite replaces the graded image.
pub(crate) const RVA_TONEMAP_QUAD: usize = 0x0068_e880;
pub(crate) const RVA_TONEMAP_QUAD_END: usize = 0x0068_ea20;
/// Mesh object: `+0x60` VBO, `+0x64` GL IBO id (u32). Chunk meshes are looked
/// up by their index buffer in the draw thunk (mesh = `*(*(element))+8`).
pub(crate) const MESH_IBO: usize = 0x64;
// GL dispatch slots the game calls through (GLEW pointer globals / import
// table entries). Re-pointing them lets the FLIR passes draw through their own
// programs: the draw thunk sees every submitted draw, the matrix upload thunk
// captures `mat_view_proj`/`mat_world`, and the framebuffer bind thunk finds
// the G-buffer.
pub(crate) const RVA_GL_DRAW_ELEMENTS: usize = 0x00a9_6658;
pub(crate) const RVA_GL_UNIFORM_MATRIX4FV: usize = 0x00ce_61c8;
pub(crate) const RVA_GL_BIND_FRAMEBUFFER: usize = 0x00ce_85e8;
// GLEW dispatch slots used only to TRACK GL state (no driver query in the draw
// hot path). Found from glewInit's `lea rcx,"glXxx"; call wglGetProcAddress;
// mov [slot],rax` sequences (0x140995EFB, 0x140995623, 0x140994AA6, 0x140987156).
pub(crate) const RVA_GL_USE_PROGRAM: usize = 0x00ce_61e0;
/// GLEW slot of `glUniform1f` (string xref, see RE notes).
pub(crate) const RVA_GL_UNIFORM1F: usize = 0x00ce_6008;
/// GLEW slot of `glUniform3f` (light_pass_uniforms 0x1406842B0).
pub(crate) const RVA_GL_UNIFORM3F: usize = 0x00ce_60c8;
pub(crate) const RVA_GL_BIND_BUFFER: usize = 0x00ce_b348;
pub(crate) const RVA_GL_ACTIVE_TEXTURE: usize = 0x00ce_a7b0;
pub(crate) const RVA_GL_BIND_VERTEX_ARRAY: usize = 0x00ce_a1d0;
/// IAT slot of `glViewport` (OPENGL32 import; `objdump -p` import table).
pub(crate) const RVA_GL_VIEWPORT: usize = 0x00a9_6738;
/// OPENGL32 IAT glBindTexture (objdump -p).
pub(crate) const RVA_GL_BIND_TEXTURE: usize = 0x00a9_6710;
// Camera component fields (from the camera serializer).
pub(crate) const CAMERA_PIVOT: usize = 0x680;
// Scene / body containers for the per-frame mesh -> body map.
pub(crate) const SCENE_VEHICLE_CONTAINER: usize = 0x4990;
pub(crate) const SCENE_VEHICLE_SET: usize = 0x320;
pub(crate) const BODY_CHUNK_ITEMS: usize = 0x540;
/// Second (pass-B) chunk table, same layout as `BODY_CHUNK_ITEMS` (re_chunks).
pub(crate) const BODY_CHUNK_ITEMS_B: usize = 0x588;

/// Encoded temperature range carried through the colour G-buffer. Wide enough
/// that an engine block and a rocket plume keep usable resolution at 8 bits.
pub(crate) const FLIR_ENC_MIN_C: f32 = -50.0;
pub(crate) const FLIR_ENC_MAX_C: f32 = 2000.0;
/// Cold sky temperature for FLIR (clear zenith is far below ambient).
pub(crate) const FLIR_SKY_C: f32 = -60.0;
/// Maximum solar soak added to a surface facing the sun, degrees C.
pub(crate) const FLIR_SUN_SOAK_C: f32 = 25.0;
/// Depth beyond which a fragment is treated as sky rather than terrain/water.
// Just inside the depth far plane (20100 m): only cleared depth is sky. The
// moon/sun sprites (~18 km) are caught by the composite's height test.
pub(crate) const FLIR_SKY_DEPTH_M: f32 = 19_500.0;
/// Ambient used when the game's heat model is not reachable yet.
pub(crate) const FLIR_AMBIENT_FALLBACK_C: f32 = 20.0;
/// GL texture unit the block pass binds the body temperature volume to.
pub(crate) const FLIR_TEXTURE_UNIT: u32 = 7;
/// A body volume is re-uploaded at most once per this many scene submits, so a
/// block that warms continuously cannot re-upload every frame.
pub(crate) const FLIR_UPLOAD_INTERVAL_FRAMES: u64 = 2;
/// Upper bound on cached body volumes before unused ones are evicted.
pub(crate) const FLIR_GRID_TEXTURE_MAX: usize = 128;
/// A registered FLIR camera not refreshed for this long is no longer sampled for ambient.
pub(crate) const FLIR_IR_CAMERA_STALE_MS: u64 = 2000;
/// A tonemap output target that no scene submit refreshed for this long is
/// dropped (with its G-buffer pairing): at one frame per view it only outlives a
/// view that stopped rendering, and a recycled framebuffer id is never trusted.
pub(crate) const FLIR_TARGET_STALE_MS: u64 = 1000;

pub(crate) const TAU: f64 = std::f64::consts::TAU;

/// Maximum seeker steering deflection, in turns.
///
/// Elevation is bounded to 90 degrees (0.25 turns) by construction, since it
/// comes from `asin` of an up-axis component. Azimuth is circular, so the
/// steering input is clamped to the same magnitude here to keep the scan cone
/// inside the gimbal's physical deflection limit instead of letting a steering
/// command swing it a half turn away.
pub(crate) const STEER_LIMIT_TURNS: f64 = 0.25;

/// Emission model: MWIR (3-5 um) in-band blackbody radiance, the band real IR
/// missile seekers use. A 400 C furnace peaks at 4.3 um (Wien) while a 40 C
/// engine radiates almost nothing in-band, so hot sources get their realistic
/// ~356x brightness over a warm engine instead of a tame (T/400 K)^4 scaling.
/// flux = factor * L_band(T_K) * r^2 / d^2 / MWIR_REF, with MWIR_REF calibrated
/// off a 40 C / r=0.7 record at 10 m. That record actually reads ~1.16x
/// MWIR_ANCHOR_FLUX because the near-field clamp makes its surface distance
/// 9.3 m rather than 10 m.
///
/// Absolute scale is arbitrary (every consumer divides by the seeker's IR gain,
/// which is a free parameter), but it is not cosmetic: the Strength readout is
/// flux/gain clamped to 1 and Target Detect is flux >= detect_flux, so a scale
/// that puts ordinary sources near 1e-4 forces both into silly territory - the
/// reported symptom was needing gain 1e-5 to see anything, which pins Strength
/// at 1 and forces a detect threshold of ~1e-5. Raised 1000x from the original
/// 0.29 so an ordinary source reads around 1 and gain ~1 works. This is the one
/// number to tune if the whole world is too bright or too dim.
pub(crate) const MWIR_ANCHOR_FLUX: f64 = 290.0;
/// Flame temperature for the fire-group class. Flares live in the *same*
/// ring as ordinary fires (the world fire-group ring at holder+0x11d8, see
/// collect_fire_sources) and carry no separate heat signature, so a single
/// temperature has to serve both and the game's own 400 brightness scale
/// cannot be used - it would drag flares down with the wood fires. 1500 C is
/// the low end of a flare's 1500-2000 C burn and a reasonable upper bound for
/// a structure fire, so it errs bright for fires and correct for flares.
pub(crate) const FIRE_T_K: f64 = 1773.15; // ~1500 C flame/flare

/// Flame radius in metres per unit of the fire's 0..1 intensity. The game
/// sizes a fire by that intensity: FUN_140300110 sets each particle's scale to
/// 20 x intensity and the renderer multiplies by the particle's +0x44 (0.1),
/// so a full-intensity fire is about a 2 m flame and a half-strength one is
/// about 1 m.
pub(crate) const FIRE_RADIUS_PER_INTENSITY: f64 = 2.0;

/// MWIR (3-5 um) extinction by weather. Fog dominates: its droplets are close
/// to the band's wavelength, so they scatter and absorb strongly, while rain
/// drops are far larger and attenuate much less per unit of water. A fully
/// fogged world blinds a long-range seeker (1 km -> ~0.06% transmittance)
/// while barely affecting a few hundred metres.
pub(crate) const FOG_SIGMA_PER_M: f64 = 0.0075;
pub(crate) const RAIN_SIGMA_PER_M: f64 = 0.0020;
/// Fog along a sky path (the sun) is deliberately gentler than fog along a
/// line of sight to a surface source: a real seeker keeps seeing the sun as a
/// diffuse glow through fairly thick fog (forward scattering dominates), while
/// a point target washes out. Rain is left at the surface coefficient.
pub(crate) const FOG_SIGMA_SKY_PER_M: f64 = 0.0070;
/// The sun emitter is modelled 10 km away but the path that actually fogs it
/// is the weather layer, so sky paths are clamped before applying extinction.
pub(crate) const SKY_PATH_CLAMP_M: f64 = 1500.0;

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
pub(crate) const SUN_FLUX: f64 = 45.0;
pub(crate) const SUN_DIST: f64 = 10000.0;
/// Angular radius of the sun at SUN_DIST: 393 m at 10 km is 2.25 deg. That is
/// 9x the real sun (0.25 deg), sized to match the much larger sun sprite the
/// game renders in the sky rather than the physical sun. SUN_FLUX is
/// unchanged, so the bigger disc is dimmer per cell, not brighter — it stays
/// saturated at typical gains.
pub(crate) const SUN_R_EFF: f64 = 393.0;
