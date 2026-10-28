//! Collectors that turn game state into `IrSource`s: heat records, world
//! fires, volume-heat emitters and engine-exhaust effects.

const std = @import("std");
const ir = @import("ir.zig");
const memory = @import("memory.zig");
const offsets = @import("offsets.zig");
const state = @import("state.zig");
const util = @import("util.zig");

const IrSource = ir.IrSource;
const Vec3 = ir.Vec3;
const readUsizeAt = memory.readUsizeAt;
const readU32At = memory.readU32At;
const readF32At = memory.readF32At;
const readF64At = memory.readF64At;
const inPtrRange = memory.inPtrRange;

/// Read an ASCII string stored as {ptr @+0, len u32 @+8} at `base` into
/// `out`; returns the bytes read.
pub fn readPstr(base: usize, out: []u8) ?[]u8 {
    if (base < 0x10000) return null;
    const ptr = readUsizeAt(base);
    const len: usize = readU32At(base + 8);
    if (ptr < 0x10000 or len == 0 or len > out.len) return null;
    const src: [*]const u8 = @ptrFromInt(ptr);
    @memcpy(out[0..len], src[0..len]);
    return out[0..len];
}

/// Map an exhaust-emitter effect index to an exhaust temperature, or null when
/// the effect is not an engine plume. Indices come from the definition table
/// built by the game's effect registrar.
pub fn exhaustIndexTemperature(index: u32) ?f64 {
    return switch (index) {
        3, 5, 7 => offsets.EXHAUST_T_DAMAGE, // exhaust_damage_1* - the kill field
        2, 4, 6 => offsets.EXHAUST_T_PLUME, // exhaust* - the visible plume
        41, 42 => offsets.EXHAUST_T_ROCKET, // rocket, rocket_strip
        else => null,
    };
}

/// Ring header {slots, cap, start, count} read from a container.
const Ring = struct { slots: usize, cap: usize, start: usize, count: usize };

fn ring(c: usize, slots_off: usize, cap_off: usize) ?Ring {
    const r: Ring = .{
        .slots = readUsizeAt(c + slots_off),
        .cap = readU32At(c + cap_off),
        .start = readU32At(c + cap_off + 4),
        .count = readU32At(c + cap_off + 8),
    };
    if (r.slots < 0x10000 or r.cap == 0 or r.count == 0) return null;
    return r;
}

fn distance(origin: *const Vec3, x: f64, y: f64, z: f64) f64 {
    const dx = x - origin[0];
    const dy = y - origin[1];
    const dz = z - origin[2];
    return @sqrt(dx * dx + dy * dy + dz * dz);
}

/// Walk a heat container and emit every record as an IrSource, applying the
/// radiometric falloff. Records are evaluated regardless of their owner (the
/// game's own evaluation filters by owner, which would hide other vehicles).
/// The game (temperature_eval_at_pos 0x14030FD60 via 0x14030BD10) reads a
/// record as ambient + factor(+0x114) * temp(+0x118), so +0x118 is an excess
/// over the air, not an absolute temperature.
pub fn collectIrSources(c: usize, origin: *const Vec3, range: f64, ambient_c: f64, out: *std.ArrayList(IrSource)) void {
    if (c < 0x10000) return;
    const r = ring(c, 0x20, 0x28) orelse return;
    const weather = ir.weatherFogRain();
    for (0..r.count) |i| {
        const idx = (r.start + i) % r.cap;
        const holder = readUsizeAt(r.slots + idx * 8);
        if (!inPtrRange(holder)) continue;
        const rec = readUsizeAt(holder);
        if (!inPtrRange(rec)) continue;
        const mode = memory.peek(u8, rec);
        const factor: f64 = readF32At(rec + 0x114);
        const temp: f64 = readF32At(rec + 0x118);
        if (factor <= 0.0 or temp <= 0.0) continue;
        var cx: f64 = undefined;
        var cy: f64 = undefined;
        var cz: f64 = undefined;
        var r_eff: f64 = undefined;
        if (mode != 0) {
            // Oriented box: matrix at +0x88 (translation column +0xe8/+0xf0/+0xf8),
            // full extents at +0x108/+0x10c/+0x110.
            const e0: f64 = readF32At(rec + 0x108);
            const e1: f64 = readF32At(rec + 0x10c);
            const e2: f64 = readF32At(rec + 0x110);
            cx = readF64At(rec + 0xe8);
            cy = readF64At(rec + 0xf0);
            cz = readF64At(rec + 0xf8);
            r_eff = 0.5 * util.fmax(util.fmax(e0, e1), e2);
        } else {
            cx = readF64At(rec + 0x68);
            cy = readF64At(rec + 0x70);
            cz = readF64At(rec + 0x78);
            r_eff = readF32At(rec + 0x108);
        }
        if (r_eff <= 0.001) continue;
        const dist = distance(origin, cx, cy, cz);
        const surface = util.fmax(dist - r_eff, 1.0);
        if (surface > range) continue;
        const temp_c = ambient_c + util.fmin(factor, 1.0) * temp;
        const t_k = temp_c + 273.15;
        const flux = ir.bandRadiance(t_k) * (r_eff * r_eff) / (surface * surface) / ir.mwirRef() *
            ir.weatherTransmittance(weather[0], weather[1], surface, false);
        out.append(state.gpa, .{
            .x = cx,
            .y = cy,
            .z = cz,
            .r_eff = r_eff,
            .flux = flux,
            .theta = std.math.atan(r_eff / util.fmax(dist, 0.5)),
            .sky = false,
            .temp_c = temp_c,
        }) catch return;
    }
}

/// Fires (burning objects) as IR emitters. Container layout is what
/// FUN_14030fd60 reads as param_5: ring slots +0x11d8, cap +0x11e0, start
/// +0x11e4, count +0x11e8; each group holds a world position at +0x38..+0x48
/// an intensity float at +0x94, an emitting flag at +0x90 and a smoke flag at +0x91. The group is
/// allocated as exactly 0x98 bytes (FUN_1403025e0), so +0x94 is its last
/// field, and FUN_140300110 clamps that intensity to 0..1 - it is a normalised
/// fire strength, not a radiance. Treated here as a FIRE_T_K emitter whose
/// radius is FIRE_RADIUS_PER_INTENSITY x intensity, matching the game's own
/// particle geometry (see those constants).
pub fn collectFireSources(c: usize, origin: *const Vec3, range: f64, out: *std.ArrayList(IrSource)) void {
    if (c < 0x10000) return;
    const r = ring(c, 0x11d8, 0x11e0) orelse return;
    const weather = ir.weatherFogRain();
    const n = @min(r.count, 256);
    for (0..n) |i| {
        const idx = (r.start + i) % r.cap;
        const group = readUsizeAt(r.slots + idx * 8);
        if (!inPtrRange(group)) continue;
        const x = readF64At(group + 0x38);
        const y = readF64At(group + 0x40);
        const z = readF64At(group + 0x48);
        const intensity: f64 = readF32At(group + 0x94);
        if (!(x != 0.0 or y != 0.0 or z != 0.0) or intensity <= 0.0) continue;
        // Positive floats compare consistently as raw bits, so fetchMax on
        // the bit pattern tracks the largest intensity. +0x91 != 0 is smoke
        // (fire_entry_set_pos_and_intensity 0x140300110 tints it 50 instead of
        // 400); +0x90 is "still emitting", cleared by fire_object_state_tick
        // 0x14030AE70 when the fire burns out and only its particles linger.
        // Smoke and lingering particles are cool.
        const bits: u32 = @bitCast(@as(f32, @floatCast(intensity)));
        _ = state.FIRE_INTENSITY_MAX_BITS.fetchMax(bits, .monotonic);
        const smoke = readU32At(group + 0x91) & 0xff != 0;
        if (smoke) {
            _ = state.SMOKE_ONLY_MAX_BITS.fetchMax(bits, .monotonic);
        } else {
            _ = state.FIRE_ONLY_MAX_BITS.fetchMax(bits, .monotonic);
        }
        if (smoke or readU32At(group + 0x90) & 0xff == 0) continue;
        const dist = distance(origin, x, y, z);
        const r_eff = offsets.FIRE_RADIUS_PER_INTENSITY * intensity;
        const surface = util.fmax(dist - r_eff, 1.0);
        if (surface > range) continue;
        const flux = intensity * ir.bandRadiance(offsets.FIRE_T_K) * (r_eff * r_eff) / (surface * surface) /
            ir.mwirRef() * ir.weatherTransmittance(weather[0], weather[1], surface, false);
        out.append(state.gpa, .{
            .x = x,
            .y = y,
            .z = z,
            .r_eff = r_eff,
            .flux = flux,
            .theta = std.math.atan(r_eff / util.fmax(dist, 0.5)),
            .sky = false,
            .temp_c = offsets.FIRE_T_K - 273.15,
        }) catch return;
    }
}

/// Spherical heat emitters on the heat container's *second* ring.
/// FUN_14030fd60 reads this ring (header +0x08 slots, +0x10 cap, +0x14 start, +0x18 count;
/// slot -> holder -> record) with a (dist - radius)/5 falloff per record:
/// heat fraction at +0x00, radius at +0x04 and world position doubles at
/// +0x40/+0x48/+0x50. The game lerps ambient toward 400 C by that fraction
/// (`base + (min(400, base + 400) - base) * heat`), so a fully burning
/// component reads 400 C. Burning vehicle components and body fires land here
/// rather than in the world fire-group ring.
pub fn collectVolumeHeatSources(c: usize, origin: *const Vec3, range: f64, ambient_c: f64, out: *std.ArrayList(IrSource)) void {
    if (c < 0x10000) return;
    const r = ring(c, 0x8, 0x10) orelse return;
    const weather = ir.weatherFogRain();
    const n = @min(r.count, 512);
    for (0..n) |i| {
        const idx = (r.start + i) % r.cap;
        const holder = readUsizeAt(r.slots + idx * 8);
        if (!inPtrRange(holder)) continue;
        const rec = readUsizeAt(holder);
        if (!inPtrRange(rec)) continue;
        const heat: f64 = readF32At(rec);
        if (heat <= 0.0) continue;
        const x = readF64At(rec + 0x40);
        const y = readF64At(rec + 0x48);
        const z = readF64At(rec + 0x50);
        if (!(x != 0.0 or y != 0.0 or z != 0.0)) continue;
        // Zero-radius records exist as points; give them a person-sized
        // emitting area so the flux model has something to work with.
        const radius: f64 = readF32At(rec + 4);
        const r_eff: f64 = if (radius > 0.05) radius else 0.3;
        const dist = distance(origin, x, y, z);
        const surface = util.fmax(dist - r_eff, 1.0);
        if (surface > range) continue;
        const temp_c = ambient_c + (util.fmin(ambient_c + 400.0, 400.0) - ambient_c) * util.fmin(heat, 1.0);
        const t_k = temp_c + 273.15;
        const flux = ir.bandRadiance(t_k) * (r_eff * r_eff) / (surface * surface) / ir.mwirRef() *
            ir.weatherTransmittance(weather[0], weather[1], surface, false);
        out.append(state.gpa, .{
            .x = x,
            .y = y,
            .z = z,
            .r_eff = r_eff,
            .flux = flux,
            .theta = std.math.atan(r_eff / util.fmax(dist, 0.5)),
            .sky = false,
            .temp_c = temp_c,
        }) catch return;
    }
}

/// Engine exhaust emitters captured from the per-tick plume update. Every
/// propulsion component updates its exhaust emitter with the nozzle's world
/// position and a throttle-scaled emission rate each tick, so the spectrum
/// follows throttle without touching game state.
pub fn collectEmitterSources(origin: *const Vec3, range: f64, out: *std.ArrayList(IrSource)) void {
    // Copy, never drain: blocktemp reads the same entries every scene tick, and a
    // seeker scan that emptied the list starved it (jets with IR seekers never heated).
    // A despawned component never ticks again, so stale entries are aged out here.
    const now = state.nowMs();
    var pending: std.ArrayList(state.Emitter) = .empty;
    defer pending.deinit(state.gpa);
    {
        const list = state.EMITTER_PENDING.lock();
        defer state.EMITTER_PENDING.unlock();
        retainFresh(list, now);
        pending.appendSlice(state.gpa, list.items) catch return;
    }
    if (pending.items.len == 0) return;
    const weather = ir.weatherFogRain();
    // Only collapse repeats of *this* collector's own emitters. Comparing
    // against the whole accumulated list let a heat record (collected just
    // before this, at the same position as the engine) suppress the engine's
    // own emitter as a "duplicate", silently hiding every engine that also has
    // a heat record - which is precisely the case this plugin exists to image.
    const first_new = out.items.len;
    for (pending.items) |e| {
        const dist = distance(origin, e.x, e.y, e.z);
        // Plume radius follows the block size (a 1x1 booster must not radiate
        // like a 3x3 motor), narrowed at low throttle.
        const r_eff = util.fclamp(
            offsets.ENGINE_R_BLOCK_FACTOR * e.size_m * (0.5 + 0.5 * util.fmin(e.activity, 1.0)),
            offsets.ENGINE_R_MIN_M,
            offsets.ENGINE_R_MAX_M,
        );
        const surface = util.fmax(dist - r_eff, 1.0);
        if (surface > range) continue;
        const flux = ir.bandRadiance(e.temp_c + 273.15) * (r_eff * r_eff) / (surface * surface) / ir.mwirRef() *
            ir.weatherTransmittance(weather[0], weather[1], surface, false);
        const cand: IrSource = .{
            .x = e.x,
            .y = e.y,
            .z = e.z,
            .r_eff = r_eff,
            .flux = flux,
            .theta = std.math.atan(r_eff / util.fmax(dist, 0.5)),
            .sky = false,
            .temp_c = e.temp_c,
        };
        // One emitter can spawn a fresh instance every frame; collapse repeats
        // onto the HOTTEST member of each 2 m cluster, not the first one pushed,
        // so a multiblock always reads as its combustion chamber rather than
        // whichever part happened to tick first.
        const hit: ?usize = for (out.items[first_new..], 0..) |s, i| {
            const ddx = s.x - e.x;
            const ddy = s.y - e.y;
            const ddz = s.z - e.z;
            if (ddx * ddx + ddy * ddy + ddz * ddz < 4.0) break i;
        } else null;
        if (hit) |i| {
            const slot = &out.items[first_new + i];
            if (cand.temp_c > slot.temp_c) slot.* = cand;
        } else {
            out.append(state.gpa, cand) catch return;
        }
    }
}

/// Drop emitters whose stamp stopped advancing (despawned components).
pub fn retainFresh(list: *std.ArrayList(state.Emitter), now: u64) void {
    var keep: usize = 0;
    for (list.items) |entry| {
        if (now -| entry.seen_ms < state.EMITTER_STALE_MS) {
            list.items[keep] = entry;
            keep += 1;
        }
    }
    list.shrinkRetainingCapacity(keep);
}

/// Drop the emitter keyed `key`.
pub fn removeEmitter(list: *std.ArrayList(state.Emitter), key: usize) void {
    var keep: usize = 0;
    for (list.items) |entry| {
        if (entry.key != key) {
            list.items[keep] = entry;
            keep += 1;
        }
    }
    list.shrinkRetainingCapacity(keep);
}
