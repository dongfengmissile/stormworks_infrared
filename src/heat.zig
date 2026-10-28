//! The game's heat model: the world root the temperature evaluators are called
//! with, the point temperature and the ambient base. Also the verbose container
//! dumps.

const std = @import("std");
const ir = @import("ir.zig");
const logging = @import("logging.zig");
const memory = @import("memory.zig");
const offsets = @import("offsets.zig");
const state = @import("state.zig");
const fault = @import("fault.zig");

const readable = memory.readable;
const readUsizeAt = memory.readUsizeAt;
const readU32At = memory.readU32At;
const readF32At = memory.readF32At;
const readF64At = memory.readF64At;
const PTR_MIN = memory.PTR_MIN;
const inPtrRange = memory.inPtrRange;
const logf = logging.logf;

/// Diagnostic dump of one heat container variant (verbose builds only): the
/// cached heat-source container (ring list) and a few records, so we can tell
/// whether a nearby engine is even a heat-field source.
pub fn logHeatContainer(c: usize) void {
    if (!logging.verbose) return;
    if (state.DEEP_DUMP_DISABLED.load(.monotonic)) return;
    // c can be a stale captured hook argument and every pointer below is
    // game-controlled, so EVERY dereference must be readable()-guarded. A
    // range check alone passes on any garbage pointer inside userland.
    if (c < PTR_MIN or !readable(c, 0x34)) {
        logging.logLine("heat container: NULL or unreadable");
        return;
    }
    const slots = readUsizeAt(c + 0x20);
    const cap = readU32At(c + 0x28);
    const start = readU32At(c + 0x2c);
    const count = readU32At(c + 0x30);
    logf("heat container 0x{x}: slots=0x{x} cap={d} start={d} count={d}", .{ c, slots, cap, start, count });
    if (slots < PTR_MIN or cap == 0 or count == 0) return;
    const dump = @min(count, 32);
    for (0..dump) |i| {
        const idx = (@as(usize, start) + i) % cap;
        if (!readable(slots + idx * 8, 8)) continue;
        const holder = readUsizeAt(slots + idx * 8);
        if (!inPtrRange(holder) or !readable(holder, 8)) continue;
        const rec = readUsizeAt(holder);
        if (!inPtrRange(rec) or !readable(rec, 0x128)) continue;
        const mode = memory.peek(u8, rec);
        const cx = readF64At(rec + 0x68);
        const cy = readF64At(rec + 0x70);
        const cz = readF64At(rec + 0x78);
        const radius = readF32At(rec + 0x108);
        const ext1 = readF32At(rec + 0x10c);
        const ext2 = readF32At(rec + 0x110);
        const factor = readF32At(rec + 0x114);
        const temp = readF32At(rec + 0x118);
        const owner = readUsizeAt(rec + 0x120);
        if (mode != 0) {
            // Box mode: print all three full extents (e0 is at +0x108) so the
            // component type can be fingerprinted from the box shape.
            logf("  heat[{d}] mode={d} c=({d:.1},{d:.1},{d:.1}) ext=({d:.2},{d:.2},{d:.2}) f={d:.3} scale={d:.1} owner=0x{x}", .{ idx, mode, cx, cy, cz, radius, ext1, ext2, factor, temp, owner });
        } else {
            logf("  heat[{d}] mode={d} c=({d:.1},{d:.1},{d:.1}) r={d:.2} f={d:.3} scale={d:.1} owner=0x{x}", .{ idx, mode, cx, cy, cz, radius, factor, temp, owner });
        }
    }
}

pub fn cacheVariant(args: *const [8]usize) void {
    if (args[0] == 0) return;
    const count = @min(state.VARIANT_COUNT.load(.monotonic), state.MAX_VARIANTS);
    for (0..count) |i| {
        const base = i * 8;
        if (state.VARIANT_ARGS[base].load(.monotonic) == args[0]) {
            for (0..8) |j| state.VARIANT_ARGS[base + j].store(args[j], .monotonic);
            return;
        }
    }
    const slot = count % state.MAX_VARIANTS;
    const base = slot * 8;
    for (0..8) |j| state.VARIANT_ARGS[base + j].store(args[j], .monotonic);
    if (count < state.MAX_VARIANTS) state.VARIANT_COUNT.store(count + 1, .monotonic);
}

pub fn variantArgs(i: usize) [8]usize {
    const base = i * 8;
    var a: [8]usize = undefined;
    for (0..8) |j| a[j] = state.VARIANT_ARGS[base + j].load(.monotonic);
    return a;
}

// ---------------------------------------------------------------------------
// World root
// ---------------------------------------------------------------------------

// Offsets of the world fields the game's temperature evaluators receive as
// arguments. The root itself is recovered once (through one of them, or from
// the scene the hooks cache) and re-validated on every use, so a call-scoped
// hook argument can never become a cached call argument again.
const WORLD_HEAT_FIELD: usize = 0xbaef0;
const WORLD_WIND_FIELD: usize = 0x68a0;
const WORLD_WEATHER: usize = 0xbade8;
const WORLD_ENV_LIST: usize = 0x4ce0;
const WORLD_PROVIDER: usize = 0x4988;

/// Water flag the game's callers pass for a craft/interior position (1), which
/// is where a seeker camera or a vehicle body always sits.
const EVAL_WATER_FLAG: u8 = 1;

/// Returned while no validated world root exists (menu, level load/teardown).
pub const WORLD_FALLBACK_C: f32 = 20.0;

var WORLD_ROOT: std.atomic.Value(usize) = .init(0);
var WORLD_LOGGED: std.atomic.Value(bool) = .init(false);
var WORLD_FAIL_LOGGED: std.atomic.Value(bool) = .init(false);

/// Validate a world root: the weather grid header, the heat field and the wind
/// field are the structures both evaluators dereference through it. A torn-down
/// world must fail this instead of being called.
fn worldOk(world_root: usize) bool {
    // Userland, 8-aligned, and low enough that every offset below cannot wrap.
    if (!(world_root >= PTR_MIN and world_root < 0x8000_0000_0000) or world_root & 7 != 0) return false;
    if (!readable(world_root + WORLD_WEATHER, 0xb8) or
        !readable(world_root + WORLD_HEAT_FIELD, 8) or
        !readable(world_root + WORLD_WIND_FIELD, 8))
    {
        return false;
    }
    // Weather grid: uint16 cell count at +6 (air/sea cell tables at +0/+0x28 and
    // the height range at +0xb0/+0xb4, docs/RE_NOTES.md §3.1). The neighbouring
    // pointer fields are cell DATA addresses inside the grid, not userland
    // pointers: they are only ever dereferenced by the evaluator itself, so
    // validating them here would reject every real world.
    if (@as(u16, @truncate(readU32At(world_root + WORLD_WEATHER + 4) >> 16)) < 2) return false;
    return readUsizeAt(world_root + WORLD_HEAT_FIELD) != 0;
}

/// Drop the cached world root (the scene object changed: world load/unload).
pub fn forgetWorld() void {
    WORLD_ROOT.store(0, .monotonic);
}

/// The validated world root, or null while the game has none. Cached once,
/// but re-derived whenever the cached value stops validating so a level reload
/// does not leave every probe on the fallback.
///
/// The cached scene pointer is preferred: the scene tick and the render uniform
/// fill both hand it over every frame, so it is the freshest copy of the same
/// object the evaluator's arguments point at.
pub fn world() ?usize {
    const cached = WORLD_ROOT.load(.monotonic);
    const live = state.SCENE.load(.monotonic);
    if (cached != 0) {
        // A freed scene from the previous world can stay readable and pass
        // worldOk; the scene tick's current object is the authority.
        if ((live == 0 or live == cached) and worldOk(cached)) return cached;
        WORLD_ROOT.store(0, .monotonic);
    }
    const scene = state.SCENE.load(.monotonic);
    var found: ?usize = null;
    var source: []const u8 = "";
    if (scene != 0 and worldOk(scene)) {
        found = scene;
        source = "cached scene";
    } else {
        // Recover the root from one of the evaluator's own arguments (cached by
        // the temperature hook, never used as call arguments any more).
        const args = variantArgs(0);
        const Candidate = struct { index: usize, field: usize, name: []const u8 };
        const candidates = [_]Candidate{
            .{ .index = 0, .field = WORLD_HEAT_FIELD, .name = "temperature arg2 - 0xbaef0" },
            .{ .index = 1, .field = WORLD_WIND_FIELD, .name = "temperature arg3 - 0x68a0" },
            .{ .index = 2, .field = WORLD_WEATHER, .name = "temperature arg4 - 0xbade8" },
            .{ .index = 3, .field = WORLD_ENV_LIST, .name = "temperature arg5 - 0x4ce0" },
            .{ .index = 5, .field = WORLD_PROVIDER, .name = "temperature arg7 - 0x4988" },
        };
        for (candidates) |c| {
            if (args[c.index] >= c.field + PTR_MIN and worldOk(args[c.index] - c.field)) {
                found = args[c.index] - c.field;
                source = c.name;
                break;
            }
        }
    }
    const world_root = found orelse {
        // A failed validation must never be silent: without a root every probe
        // is the fixed fallback, which otherwise looks like a broken sim.
        if (!WORLD_FAIL_LOGGED.swap(true, .monotonic)) {
            logf("world root not found (scene=0x{x}); temperature probes report the {d} C fallback", .{ scene, WORLD_FALLBACK_C });
        }
        return null;
    };
    WORLD_ROOT.store(world_root, .monotonic);
    if (!WORLD_LOGGED.swap(true, .monotonic)) {
        logf("world root 0x{x} from {s} (scene=0x{x} same={})", .{ world_root, source, scene, world_root == scene });
    }
    return world_root;
}

/// Out parameter of `fakeProviderGet`: the evaluator hands the provider its
/// output pointer in RDX. The call site has no guard at all, so nothing here may
/// fault: return 0 C as well, and write only into memory proven writable.
fn fakeProviderGet(_: usize, out: usize, _: usize) callconv(.c) f32 {
    if (memory.writable(out, 4)) memory.poke(f32, out, 0.0);
    return 0.0;
}

// A stand-in for the vehicle air-volume provider: the evaluator reads the
// provided temperature through vtable slot +0x28 (index 5) only, so a slot that
// reports 0 C ("no room air volume here") reproduces the caller's behaviour.
const FAKE_PROVIDER_VT = [6]?*const anyopaque{ null, null, null, null, null, @ptrCast(&fakeProviderGet) };
const FakeProvider = extern struct { vtable: *const [6]?*const anyopaque, pad: usize = 0 };
const FAKE_PROVIDER: FakeProvider = .{ .vtable = &FAKE_PROVIDER_VT };

fn fakeProvider() usize {
    return @intFromPtr(&FAKE_PROVIDER);
}

/// The game's own provider object, but only when both its vtable and the slot
/// the evaluator calls are game image memory; otherwise the static stand-in.
fn providerObject(world_root: usize) usize {
    const provider = world_root + WORLD_PROVIDER;
    if (readable(provider, 8)) {
        const vtable = readUsizeAt(provider);
        if (memory.inGameImage(vtable) and readable(vtable + 0x28, 8)) {
            if (memory.inGameImage(readUsizeAt(vtable + 0x28))) return provider;
        }
    }
    return fakeProvider();
}

// ---------------------------------------------------------------------------
// evaluators
// ---------------------------------------------------------------------------

/// Base ambient temperature at a world point (no heat sources added).
///
/// `ambient_temp_at_pos(pos, windField, weatherGrid, ignored, waterFlag)`: a4 is
/// dead in the callee (only its home slot is used as scratch), so it gets a
/// local buffer instead of a cached object.
pub fn baseAmbientAt(origin: ir.Vec3) f32 {
    const world_root = world() orelse return WORLD_FALLBACK_C;
    return baseAmbientIn(world_root, origin);
}

/// `baseAmbientAt` for a caller that already holds a validated world root.
fn baseAmbientIn(world_root: usize, origin: ir.Vec3) f32 {
    const pos = [4]f64{ origin[0], origin[1], origin[2], 0.0 };
    var scratch: u64 = 0;
    const ambient_fn = memory.gameFn(*const fn (usize, usize, usize, usize, u8) callconv(.c) f32, offsets.RVA_BASE_TEMP);
    const value = ambient_fn(@intFromPtr(&pos), world_root + WORLD_WIND_FIELD, world_root + WORLD_WEATHER, @intFromPtr(&scratch), EVAL_WATER_FLAG);
    return if (std.math.isFinite(value)) value else WORLD_FALLBACK_C;
}

pub const TemperatureFn = *const fn (usize, usize, usize, usize, usize, usize, usize, u8, usize) callconv(.c) f32;

pub fn temperatureAt(x: f64, y: f64, z: f64) f32 {
    fault.set(fault.stage.HEAT_TEMPERATURE_AT);
    const world_root = world() orelse return WORLD_FALLBACK_C;
    const pos = [4]f64{ x, y, z, 0.0 };

    // The game's own evaluator (fires, lava, hot objects, the vehicle air-volume
    // provider), with every argument rebuilt from the validated world root:
    //   temperature_eval_at_pos(pos, heat, wind, weather, envList, unused,
    //                           provider, waterFlag, airVolume)
    const original = state.ORIG_TEMPERATURE.load(.seq_cst);
    const eval = if (original != 0) original else @intFromPtr(memory.gameFn(*const anyopaque, offsets.RVA_TEMPERATURE));
    if (eval == 0) return WORLD_FALLBACK_C;
    const f: TemperatureFn = @ptrFromInt(eval);
    const value = f(
        @intFromPtr(&pos),
        world_root + WORLD_HEAT_FIELD,
        world_root + WORLD_WIND_FIELD,
        world_root + WORLD_WEATHER,
        world_root + WORLD_ENV_LIST,
        0,
        providerObject(world_root),
        EVAL_WATER_FLAG,
        0,
    );
    return if (std.math.isFinite(value)) value else WORLD_FALLBACK_C;
}

/// Diagnostic dump of the fires container (verbose builds only).
pub fn logFiresContainer(c: usize) void {
    if (!logging.verbose) return;
    if (state.DEEP_DUMP_DISABLED.load(.monotonic)) return;
    if (c < PTR_MIN or !readable(c, 0x11ec)) {
        logging.logLine("fires container: NULL or unreadable");
        return;
    }
    const slots = readUsizeAt(c + 0x11d8);
    const cap = readU32At(c + 0x11e0);
    const start = readU32At(c + 0x11e4);
    const count = readU32At(c + 0x11e8);
    logf("fires container 0x{x}: slots=0x{x} cap={d} start={d} count={d}", .{ c, slots, cap, start, count });
    if (slots < PTR_MIN or cap == 0 or count == 0) return;
    const dump = @min(@as(usize, count), 4);
    for (0..dump) |i| {
        const idx = (@as(usize, start) + i) % cap;
        if (!readable(slots + idx * 8, 8)) continue;
        const group = readUsizeAt(slots + idx * 8);
        if (!inPtrRange(group) or !readable(group, 0x98)) continue;
        const x = readF64At(group + 0x38);
        const y = readF64At(group + 0x40);
        const z = readF64At(group + 0x48);
        const intensity = readF32At(group + 0x94);
        logf("  fire[{d}] c=({d:.1},{d:.1},{d:.1}) intensity={d:.3}", .{ idx, x, y, z, intensity });
    }
}

/// Diagnostic dump of the heat container's second ring (volume heat emitters).
pub fn logVolumeHeatSources(c: usize) void {
    if (!logging.verbose) return;
    if (state.DEEP_DUMP_DISABLED.load(.monotonic)) return;
    if (c < PTR_MIN or !readable(c, 0x1c)) return;
    const slots = readUsizeAt(c + 0x8);
    const cap = readU32At(c + 0x10);
    const start = readU32At(c + 0x14);
    const count = readU32At(c + 0x18);
    logf("volume-heat container 0x{x}: slots=0x{x} cap={d} start={d} count={d}", .{ c, slots, cap, start, count });
    if (slots < PTR_MIN or cap == 0 or count == 0) return;
    const dump = @min(@as(usize, count), 6);
    for (0..dump) |i| {
        const idx = (@as(usize, start) + i) % cap;
        if (!readable(slots + idx * 8, 8)) continue;
        const holder = readUsizeAt(slots + idx * 8);
        if (!inPtrRange(holder) or !readable(holder, 8)) continue;
        const rec = readUsizeAt(holder);
        if (!inPtrRange(rec) or !readable(rec, 0x58)) continue;
        logf("  vheat[{d}] c=({d:.1},{d:.1},{d:.1}) heat={d:.1} C radius={d:.2}", .{
            idx,
            readF64At(rec + 0x40),
            readF64At(rec + 0x48),
            readF64At(rec + 0x50),
            readF32At(rec),
            readF32At(rec + 4),
        });
    }
}
