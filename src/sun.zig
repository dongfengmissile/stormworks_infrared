//! The sun as an IR emitter: the game's own rotating sun direction is
//! captured from the per-tick sun update, and the day/night gate comes from
//! the live daylight factor.

const std = @import("std");
const ir = @import("ir.zig");
const memory = @import("memory.zig");
const offsets = @import("offsets.zig");
const state = @import("state.zig");
const util = @import("util.zig");

/// Day clock (uint32 minutes-ish) inside the scene object.
const SCENE_DAY_CLOCK: usize = 0xbaec0;

/// The cached scene object, or the fires holder while no scene is cached yet.
/// The fires holder has no clock, so the caller guards every field it reads:
/// nothing derived from a hook argument is used as a scene any more (that value
/// was the wind field, not the scene).
pub fn sceneEnv() usize {
    const s = state.SCENE.load(.monotonic);
    if (s >= 0x10000) return s;
    return state.FIRES_HOLDER.load(.monotonic); // last resort (has fires, but no scene clock)
}

pub const SettingsFn = *const fn () callconv(.c) usize;
pub const SunRotFn = *const fn (f32, *[4]f32, *[16]f32) callconv(.c) void;

pub const GameSun = struct {
    /// Unit vector TO the sun.
    dir: [3]f64,
    /// Day percent 0..1.
    percent: f64,
};

/// The game's sun, or null while no scene/clock is available. Game thread
/// only (reads the scene clock).
pub fn gameSun() ?GameSun {
    const env = sceneEnv();
    if (env < 0x10000) return null;
    const settings = memory.gameFn(SettingsFn, offsets.RVA_SETTINGS)();
    if (settings < 0x10000) return null;
    const day_min: u64 = memory.readU32At(settings);
    if (day_min == 0 or day_min > 24 * 60) return null;
    const day_ticks = day_min * 3600;
    // The fires-holder fallback has no clock: skip instead of reading wild
    // memory at the scene offset.
    if (!memory.readable(env + SCENE_DAY_CLOCK, 4)) return null;
    const clock: u64 = memory.readU32At(env + SCENE_DAY_CLOCK);
    const percent = util.f(clock % day_ticks) / util.f(day_ticks);
    // Camera-independent sun direction straight from the game: FUN_1402bf9e0
    // is the exact function that builds the rotating sun (it feeds the sun
    // light matrix, the sky sprite at 18 km and the 37449100 m sun ray used
    // by the sky/atmosphere code). Pure math, no game state - safe to call.
    const sunrot = memory.gameFn(SunRotFn, offsets.RVA_SUNROT);
    var gdir = [4]f32{ 0, 0, 0, 0 };
    var gmat = [_]f32{0} ** 16;
    sunrot(@floatCast(percent), &gdir, &gmat);
    const vx: f64 = gdir[0];
    const vy: f64 = gdir[1];
    const vz: f64 = gdir[2];
    const vn = @sqrt(vx * vx + vy * vy + vz * vz);
    if (!(vn > 1.0e-6)) return null;
    // The game's vector points FROM the sun (the light travels along it, and
    // the sun ray origin is camera - dir * 37449100); we want TO the sun.
    state.SUN_SCENE_LOG.store(env, .monotonic);
    state.SUN_CLOCK_LOG.store(@intCast(clock), .monotonic);
    return .{ .dir = .{ -vx / vn, -vy / vn, -vz / vn }, .percent = percent };
}

/// The game draws its sun sprite lower than its light direction: measured in
/// game (08/12/16 h) as to_sun shifted down by this much before normalising
/// (1.85 deg low at 65 deg elevation, ~3.5 deg at 27 deg, none sideways).
/// shortcut: empirical fit from screenshots, replace with the sprite's real
/// placement once the sun draw is reverse engineered.
pub const SUN_SPRITE_DROP: f64 = 0.07;

/// Where the player sees the sun, from the game's to-sun light direction.
pub fn visibleSunDir(to_sun: [3]f64) [3]f64 {
    const v = [3]f64{ to_sun[0], to_sun[1] - SUN_SPRITE_DROP, to_sun[2] };
    const n = @sqrt(v[0] * v[0] + v[1] * v[1] + v[2] * v[2]);
    return .{ v[0] / n, v[1] / n, v[2] / n };
}

pub fn addSunSource(origin: ir.Vec3, sources: *std.ArrayList(ir.IrSource)) void {
    state.SUN_LOG.store(0, .monotonic);
    const sun = gameSun() orelse return; // no scene yet (dump prints "waiting")
    // The seeker sees the sun where it is drawn.
    const d = visibleSunDir(sun.dir);
    const el = std.math.asin(d[1]);
    if (el < 0.0) {
        state.SUN_LOG.store(1, .monotonic); // 1 = sun below the horizon
        return;
    }
    const az_deg = util.degrees(std.math.atan2(d[2], d[0]));
    state.SUN_EL.store(util.f2i(isize, util.degrees(el) * 10.0), .monotonic);
    state.SUN_AZ.store(util.f2i(isize, az_deg * 10.0), .monotonic);
    const pct = util.f2i(usize, util.fclamp(sun.percent * 1000.0, 0.0, 1_000_000.0));
    state.SUN_PCT.store(pct, .monotonic);
    const weather = ir.weatherFogRain();
    sources.append(state.gpa, .{
        .x = origin[0] + d[0] * offsets.SUN_DIST,
        .y = origin[1] + d[1] * offsets.SUN_DIST,
        .z = origin[2] + d[2] * offsets.SUN_DIST,
        .r_eff = offsets.SUN_R_EFF,
        .flux = offsets.SUN_FLUX * ir.weatherTransmittance(weather[0], weather[1], offsets.SUN_DIST, true),
        .theta = std.math.atan(offsets.SUN_R_EFF / offsets.SUN_DIST),
        .sky = true,
        .temp_c = 0.0,
    }) catch return;
    state.SUN_LOG.store(pct + 2, .monotonic);
}

test "visible sun matches measured drop" {
    const el = struct {
        fn of(d: [3]f64) f64 {
            return util.degrees(std.math.asin(d[1]));
        }
        fn at(deg: f64) [3]f64 {
            return .{ @cos(util.radians(deg)), @sin(util.radians(deg)), 0.0 };
        }
    };
    const noon = el.of(el.at(65.4)) - el.of(visibleSunDir(el.at(65.4)));
    const morning = el.of(el.at(27.5)) - el.of(visibleSunDir(el.at(27.5)));
    try std.testing.expect(@abs(noon - 1.85) < 0.3);
    try std.testing.expect(@abs(morning - 3.5) < 0.5);
}
