//! Reverse-engineered game addresses, byte-signature gates and tuning
//! constants. Everything here is a fact about `stormworks64.exe`; keep it in
//! one place so a game update is a single-file diff.

/// Build this DLL was relocated against.
/// stormworks64.exe sha256:
/// f9206d85c82f4d02fd0ac391781d19c5c68394a9ffc48accca0e1f5966db8699
pub(crate) const GAME_BUILD_LABEL: &str = "f9206d85c82f";

// RVAs (image base 0x140000000).
pub(crate) const RVA_COMPONENT_LUA_REGISTRAR: usize = 0x0032_be50; // FUN_14032be50
// The screen/microcontroller Lua initializer: it builds the `debug`, `async` and
// `screen` tables, so it is the state an on-screen image has to be drawn from.
// The component-mod registrar above only reaches component scripts, which cannot
// draw, and its state has no screen API.
pub(crate) const RVA_LUA_SCREEN_INIT: usize = 0x002e_5230; // FUN_1402e5230
pub(crate) const RVA_TEMPERATURE: usize = 0x0030_fd60; // FUN_14030fd60
pub(crate) const RVA_ARG_SLOT: usize = 0x0009_7920; // FUN_140097920
pub(crate) const RVA_PUSH_STRING: usize = 0x0009_7f60; // FUN_140097f60
pub(crate) const RVA_CREATE_TABLE: usize = 0x000b_a830; // FUN_1400ba830
pub(crate) const RVA_RAWSETI: usize = 0x0009_8570; // FUN_140098570
pub(crate) const RVA_REGISTER_TABLE: usize = 0x001e_22d0; // FUN_1401e22d0
pub(crate) const RVA_STRING_ALLOC: usize = 0x0017_cbb0; // FUN_14017cbb0
pub(crate) const RVA_STRING_FILL: usize = 0x0099_d3b0; // FUN_14099d3b0
pub(crate) const RVA_BASE_TEMP: usize = 0x0031_00a0; // FUN_1403100a0(point,..,daybyte)->ambient
pub(crate) const RVA_RAYCAST: usize = 0x001a_4160; // FUN_1401a4160(ctx,seg6d,out_hit,out_obj,grp,mask)->bool
pub(crate) const RVA_FIRES_BUILD: usize = 0x0030_a2f0; // FUN_14030a2f0 fire vertex builder (holder owns the fires ring)
pub(crate) const RVA_SCENE_TICK: usize = 0x005e_85e0; // FUN_1405e85e0 scene tick (arg1 = scene object; refreshes the day clock)
pub(crate) const RVA_SETTINGS: usize = 0x0029_4520; // FUN_140294520() -> settings singleton ([0] = day length, minutes)
pub(crate) const RVA_FRAME_UNIFORMS: usize = 0x006b_b400; // FUN_1406bb400 render-thread uniform fill; arg2 = the scene object
pub(crate) const FRAME_UNIFORMS_PROLOGUE: [u8; 32] = [
    0x40, 0x55, 0x53, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48, 0x8d, 0xac,
    0x24, 0xc8, 0x77, 0xff, 0xff, 0xb8, 0x58, 0x89, 0x00, 0x00, 0xe8, 0xf1, 0x77, 0x31, 0x00, 0x48,
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
pub(crate) const RVA_ROCKET_TICK: usize = 0x0043_7570; // FUN_140437570
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
pub(crate) const RVA_PRESET_ENGINE_TICK: usize = 0x0034_ba00; // FUN_14034ba00
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
pub(crate) const RVA_MODULAR_ENGINE_TICK: usize = 0x0044_9670; // FUN_140449670
pub(crate) const MODULAR_ENGINE_TICK_PROLOGUE: [u8; 32] = [
    0x48, 0x8b, 0xc4, 0x48, 0x89, 0x58, 0x10, 0x48, 0x89, 0x70, 0x18, 0x55, 0x57, 0x41, 0x54, 0x41,
    0x56, 0x41, 0x57, 0x48, 0x8d, 0xa8, 0x58, 0xfe, 0xff, 0xff, 0x48, 0x81, 0xec, 0x80, 0x02, 0x00,
];
// Fluid/heat machinery (part type 24: radiators, heat exchangers, tanks,
// valves, pumps...). Type 24 spans several component classes, so the two
// heat-relevant ones are probed: the water class (0x140b08f18) and the steam
// class (0x140af6dc8). Both ticks take the component in RCX like the engine
// ticks.
pub(crate) const RVA_WATER_TICK: usize = 0x003e_f860; // FUN_1403ef860
pub(crate) const WATER_TICK_PROLOGUE: [u8; 32] = [
    0x48, 0x8b, 0xc4, 0x48, 0x89, 0x58, 0x10, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56,
    0x41, 0x57, 0x48, 0x8d, 0xa8, 0x98, 0xf9, 0xff, 0xff, 0x48, 0x81, 0xec, 0x30, 0x07, 0x00, 0x00,
];
pub(crate) const RVA_STEAM_TICK: usize = 0x0040_cbd0; // FUN_14040cbd0
pub(crate) const STEAM_TICK_PROLOGUE: [u8; 32] = [
    0x48, 0x8b, 0xc4, 0x48, 0x89, 0x58, 0x10, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56,
    0x41, 0x57, 0x48, 0x8d, 0xa8, 0xf8, 0xfb, 0xff, 0xff, 0x48, 0x81, 0xec, 0xd0, 0x04, 0x00, 0x00,
];
/// Diesel exhaust temperature range (provisional; the block runs far cooler
/// than a jet or rocket plume).
pub(crate) const DIESEL_T_MIN_C: f64 = 80.0;
pub(crate) const DIESEL_T_MAX_C: f64 = 520.0;

pub(crate) const RVA_JET_TICK: usize = 0x0035_0f40; // FUN_140350f40
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
pub(crate) const JET_T_MIN_C: f64 = 150.0;
pub(crate) const JET_T_MAX_C: f64 = 900.0;
pub(crate) const RVA_EXHAUST_UPDATE: usize = 0x0030_2bb0; // FUN_140302bb0(emitter, world_pos, ..., rate, ...)
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
pub(crate) const RVA_SUNROT: usize = 0x002bf_9e0; // FUN_1402bf9e0(day_percent_f32, dir3_out, mat_out) — the game's rotating sun
pub(crate) const RVA_SUN_UPDATE: usize = 0x005e_3270; // FUN_1405e3270(ctx, day_percent, flags, out) — per-tick sun update; out+0x198 = sun sprite world position
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
    0x00, 0x00, 0x48, 0x8b, 0x08, 0x8b, 0x04, 0x0a, 0x39, 0x05, 0xa6, 0x8c, 0xa6, 0x00, 0x7e, 0x33,
];
pub(crate) const SCENE_TICK_PROLOGUE: [u8; 32] = [
    0x48, 0x89, 0x5c, 0x24, 0x08, 0x48, 0x89, 0x6c, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x57,
    0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48, 0x83, 0xec, 0x20, 0x80, 0x3d, 0x51, 0x1e,
];

pub(crate) const TAU: f64 = std::f64::consts::TAU;

/// Emission model: MWIR (3-5 um) in-band blackbody radiance, the band real IR
/// missile seekers use. A 400 C furnace peaks at 4.3 um (Wien) while a 40 C
/// engine radiates almost nothing in-band, so hot sources get their realistic
/// ~356x brightness over a warm engine instead of a tame (T/400 K)^4 scaling.
/// flux = factor * L_band(T_K) * r^2 / d^2 / MWIR_REF, with MWIR_REF calibrated
/// so a 40 C / r=0.7 record at 10 m reads ~0.29 (the old model's anchor).
pub(crate) const MWIR_ANCHOR_FLUX: f64 = 0.29;
pub(crate) const FIRE_T_K: f64 = 1273.15; // ~1000 C flame, scaled by the fire's intensity

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
pub(crate) const SUN_R_EFF: f64 = 44.0; // ~0.25 deg angular size at 10 km
