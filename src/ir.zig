//! The sensor model itself: radiometry (MWIR band radiance), atmospheric
//! attenuation, source projection onto the array, and the physics raycast
//! used for occlusion.

const std = @import("std");
const memory = @import("memory.zig");
const offsets = @import("offsets.zig");
const state = @import("state.zig");
const util = @import("util.zig");

/// A world-space point.
pub const Vec3 = [3]f64;

pub const IrSource = struct {
    x: f64,
    y: f64,
    z: f64,
    r_eff: f64, // effective emitting radius (m)
    flux: f64, // irradiance at the sensor, 1.0 == 127 C / 1 m source at 10 m
    az: f64 = 0, // direction in the sensor frame (rad)
    el: f64 = 0,
    theta: f64, // angular radius (rad)
    sky: bool, // sky source (the sun): not range-culled
    temp_c: f64, // emitter temperature (C), for attribution only
};

/// Planck spectral radiance integrated over 3..5 um (W/m^2/sr), 24-point
/// Simpson. Cheap: evaluated once per source per scan, not per cell.
pub fn bandRadiance(t_k: f64) f64 {
    const C1: f64 = 1.19104e-16; // 2 h c^2
    const C2: f64 = 1.4388e-2; // h c / k
    if (!(t_k >= 100.0 and t_k <= 4000.0)) return 0.0;
    const a: f64 = 3e-6;
    const b: f64 = 5e-6;
    const n: usize = 24; // even
    const h = (b - a) / @as(f64, n);
    var s: f64 = 0.0;
    for (0..n + 1) |i| {
        const lam = a + @as(f64, @floatFromInt(i)) * h;
        const x = C2 / (lam * t_k);
        const lam5 = lam * lam * lam * lam * lam;
        const value = C1 / lam5 / std.math.expm1(x);
        const w: f64 = if (i == 0 or i == n) 1.0 else if (i % 2 == 1) 4.0 else 2.0;
        s += value * w;
    }
    return s * h / 3.0;
}

/// `bandRadiance` from a 1 C lookup table (-80..1200 C, linear between
/// entries): cheap enough to evaluate for every vehicle cell.
pub fn bandRadianceC(t_c: f32) f64 {
    const table = radiance_table.get();
    if (!std.math.isFinite(t_c)) return 0.0;
    const x = util.fclamp(@as(f64, t_c) + 80.0, 0.0, 1280.0);
    const i: usize = @min(util.f2i(usize, x), 1279);
    const frac = x - @as(f64, @floatFromInt(i));
    return table[i] * (1.0 - frac) + table[i + 1] * frac;
}

/// A value computed once on first use (thread-safe; a racing second thread
/// computes the same value and stores it again).
fn Lazy(comptime T: type, comptime init: fn () T) type {
    return struct {
        value: T = undefined,
        ready: std.atomic.Value(bool) = .init(false),

        fn get(self: *@This()) *const T {
            if (!self.ready.load(.acquire)) {
                self.value = init();
                self.ready.store(true, .release);
            }
            return &self.value;
        }
    };
}

fn buildRadianceTable() [1281]f64 {
    var table: [1281]f64 = undefined;
    for (&table, 0..) |*slot, i| slot.* = bandRadiance(@as(f64, @floatFromInt(i)) - 80.0 + 273.15);
    return table;
}

var radiance_table: Lazy([1281]f64, buildRadianceTable) = .{};

/// Local weather. The game keeps its weather object on the scene at
/// scene+0xbade8 (the day clock sits just after it at +0xbaec0): the live,
/// smoothed fog factor is at +0x90 and rain at +0x98, while the cheat menu /
/// server.setWeather write target values at +0x94 / +0x9c which the live
/// values chase by 1% per tick (see FUN_1402c0810 / FUN_1402c1400). Reading
/// the max of live and target covers the transition too. Wind is at
/// +0xa0 (live) / +0xa4 (target), direction at +0xa8.
pub fn weatherFogRain() struct { f32, f32 } {
    const scene = state.SCENE.load(.monotonic);
    if (scene < 0x10000) return .{ 0.0, 0.0 };
    const w = scene + 0xbade8;
    const fog = util.fmax(memory.readF32At(w + 0x90), memory.readF32At(w + 0x94));
    const rain = util.fmax(memory.readF32At(w + 0x98), memory.readF32At(w + 0x9c));
    return .{
        if (std.math.isFinite(fog)) util.fclamp(fog, 0.0, 1.0) else 0.0,
        if (std.math.isFinite(rain)) util.fclamp(rain, 0.0, 1.0) else 0.0,
    };
}

pub fn weatherTransmittance(fog: f32, rain: f32, path_m: f64, sky: bool) f64 {
    const fog_sigma = if (sky) offsets.FOG_SIGMA_SKY_PER_M else offsets.FOG_SIGMA_PER_M;
    const sigma = @as(f64, fog) * fog_sigma + @as(f64, rain) * offsets.RAIN_SIGMA_PER_M;
    if (sigma <= 0.0) return 1.0;
    const path = if (sky) util.fmin(path_m, offsets.SKY_PATH_CLAMP_M) else path_m;
    return @exp(-sigma * path);
}

fn computeMwirRef() f64 {
    return bandRadiance(313.15) * 0.7 * 0.7 / (10.0 * 10.0) / offsets.MWIR_ANCHOR_FLUX;
}

var mwir_ref_cache: Lazy(f64, computeMwirRef) = .{};

/// Normalisation constant derived once from the 40 C anchor (see offsets).
pub fn mwirRef() f64 {
    return mwir_ref_cache.get().*;
}

/// Express a source's direction in the sensor frame (azimuth/elevation).
pub fn projectSource(s: *IrSource, origin: Vec3, forward: Vec3, right: Vec3, up: Vec3) void {
    var dx = s.x - origin[0];
    var dy = s.y - origin[1];
    var dz = s.z - origin[2];
    const len = util.fmax(@sqrt(dx * dx + dy * dy + dz * dz), 1e-6);
    dx /= len;
    dy /= len;
    dz /= len;
    const fx = dx * forward[0] + dy * forward[1] + dz * forward[2];
    const rx = dx * right[0] + dy * right[1] + dz * right[2];
    const ux = dx * up[0] + dy * up[1] + dz * up[2];
    s.az = std.math.atan2(rx, fx);
    s.el = std.math.asin(util.fclamp(ux, -1.0, 1.0));
}

/// World-space raycast through the game's physics world (Bullet rayTest).
/// Returns the distance to the first hit in metres, or null.
pub fn raycastHit(ctx: usize, origin: Vec3, target: Vec3) ?f64 {
    if (ctx == 0) return null;
    const ray_fn = memory.gameFn(*const fn (usize, *const [6]f64, *[3]f64, *usize, u32, u32) callconv(.c) u8, offsets.RVA_RAYCAST);
    const seg = [6]f64{ origin[0], origin[1], origin[2], target[0], target[1], target[2] };
    if ((seg[3] - seg[0]) * (seg[3] - seg[0]) +
        (seg[4] - seg[1]) * (seg[4] - seg[1]) +
        (seg[5] - seg[2]) * (seg[5] - seg[2]) < 1e-6)
    {
        return null;
    }
    var hit_point = [3]f64{ 0, 0, 0 };
    var hit_object: usize = 0;
    const hit = ray_fn(ctx, &seg, &hit_point, &hit_object, 0x7f, 0xff);
    if (hit == 0) return null;
    const dx = hit_point[0] - seg[0];
    const dy = hit_point[1] - seg[1];
    const dz = hit_point[2] - seg[2];
    return @sqrt(dx * dx + dy * dy + dz * dz);
}

pub fn cross(a: Vec3, b: Vec3) Vec3 {
    return .{
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    };
}

pub fn normalize(v: Vec3) Vec3 {
    const length = @sqrt(v[0] * v[0] + v[1] * v[1] + v[2] * v[2]);
    if (length < 1e-12) return .{ 0, 0, 0 };
    return .{ v[0] / length, v[1] / length, v[2] / length };
}

/// Reads the component's world transform. The 4x4 matrix is stored
/// column-major at `component + 0x1e8` (verified against `FUN_140339aa0`).
pub fn readComponentMatrix(component: usize) [16]f64 {
    const base = component + offsets.COMPONENT_MATRIX_OFFSET;
    var matrix: [16]f64 = undefined;
    for (&matrix, 0..) |*slot, index| slot.* = memory.peek(f64, base + index * 8);
    return matrix;
}

/// (forward, up) for a component-local forward axis:
/// `0=+X 1=-X 2=+Y 3=-Y 4=+Z 5=-Z`.
pub fn axesFor(axis: i32, matrix: *const [16]f64) struct { Vec3, Vec3 } {
    const x = column(matrix, 0);
    const y = column(matrix, 1);
    const z = column(matrix, 2);
    return switch (axis) {
        0 => .{ x, y },
        1 => .{ negate(x), y },
        2 => .{ y, negate(z) },
        3 => .{ negate(y), negate(z) },
        4 => .{ z, y },
        5 => .{ negate(z), y },
        else => .{ z, y },
    };
}

fn column(matrix: *const [16]f64, c: usize) Vec3 {
    return .{ matrix[c * 4], matrix[c * 4 + 1], matrix[c * 4 + 2] };
}

fn negate(v: Vec3) Vec3 {
    return .{ -v[0], -v[1], -v[2] };
}
