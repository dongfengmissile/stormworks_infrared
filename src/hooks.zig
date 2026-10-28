//! Every MinHook detour: the component Lua registrar, the temperature
//! function, the fire vertex builder, the scene tick, the sun update, the
//! render uniform fill and the exhaust-plume emitters — plus the fail-closed prologue
//! gate that refuses to install when the game build changed.

const std = @import("std");
const api = @import("api.zig");
const blocktemp = @import("blocktemp.zig");
const flir = @import("flir.zig");
const heat = @import("heat.zig");
const logging = @import("logging.zig");
const memory = @import("memory.zig");
const minhook = @import("minhook.zig");
const offsets = @import("offsets.zig");
const sources = @import("sources.zig");
const state = @import("state.zig");
const util = @import("util.zig");

const Atomic = std.atomic.Value;
const gpa = state.gpa;
const verbose = logging.verbose;
const logLine = logging.logLine;
const logf = logging.logf;
const readable = memory.readable;
const readUsizeAt = memory.readUsizeAt;
const readU32At = memory.readU32At;
const readF32At = memory.readF32At;
const readF64At = memory.readF64At;
const PTR_MIN = memory.PTR_MIN;

const ENGINE_DEFAULT_SIZE_M = offsets.ENGINE_DEFAULT_SIZE_M;
const ENGINE_MIN_POWER = offsets.ENGINE_MIN_POWER;
const EMITTER_MAX_PENDING = offsets.EMITTER_MAX_PENDING;

pub fn irRegistrarHook(context: usize) callconv(.c) void {
    const original = state.ORIG_REGISTRAR.load(.seq_cst);
    if (original != 0) {
        const f: *const fn (usize) callconv(.c) void = @ptrFromInt(original);
        f(context);
    }
    api.registerIrTable(context);
    _ = state.REGISTRAR_HITS.fetchAdd(1, .monotonic);
}

/// Screen / microcontroller Lua initializer (it builds `debug`, `async`,
/// `screen`).
///
/// `ir` is registered here as well as in the component-mod environment, because
/// a microcontroller script is where `screen.*` lives and the image therefore
/// has to be drawn from there. Only the component-independent calls are
/// exposed - see registerIrScreenTable.
pub fn irScreenInitHook(lua_state: usize) callconv(.c) void {
    const original = state.ORIG_SCREEN_INIT.load(.seq_cst);
    if (original != 0) {
        const f: *const fn (usize) callconv(.c) void = @ptrFromInt(original);
        f(lua_state);
    }
    api.registerIrScreenTable(lua_state);
    _ = state.SCREEN_INIT_HITS.fetchAdd(1, .monotonic);
}

/// Per-tick exhaust plume update: (emitter, world position, ..., rate pointer, ...).
///
/// The emitter carries its own identity: `*emitter` is the effect definition,
/// whose index sits at +0x10 and whose name string is at +0x00 (the layout the
/// effect registrar FUN_140305480 writes). That is how a plume is classified -
/// the earlier create-time map recorded the smart-pointer handle rather than the
/// emitter object, so it never matched anything.
///
/// Every argument is a pointer or a stack integer, so declaring them usize keeps
/// the ABI bit-exact with no float registers involved.
fn irExhaustUpdateHook(
    emitter: usize,
    position: usize,
    p3: usize,
    p4: usize,
    rate_ptr: usize,
    p6: usize,
    p7: usize,
    p8: usize,
    p9: usize,
    p10: usize,
    p11: usize,
) callconv(.c) f32 {
    exhaustUpdate(emitter, position, rate_ptr);
    const original = state.ORIG_EXHAUST_UPDATE.load(.seq_cst);
    if (original == 0) return 0.0;
    // Returns float (emitter+0x50 spawn accumulator) in xmm0.
    const f: *const fn (usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize) callconv(.c) f32 = @ptrFromInt(original);
    return f(emitter, position, p3, p4, rate_ptr, p6, p7, p8, p9, p10, p11);
}

var UPDATE_LOG_COUNT: Atomic(usize) = .init(0);

fn exhaustUpdate(emitter: usize, position: usize, rate_ptr: usize) void {
    if (!readable(emitter, 0x18)) return;
    const definition = readUsizeAt(emitter);
    if (!(definition >= 0x10000 and readable(definition, 0x68))) return;
    const index = readU32At(definition + 0x10);
    // The rate arrives in a stack slot. Callers do pass it, but a caller that
    // does not would leave an arbitrary value there, so probe before reading
    // rather than trusting the ABI.
    const rate: f64 = if (readable(rate_ptr, 4)) readF32At(rate_ptr) else 0.0;
    const positioned = readable(position, 24);

    if (verbose) {
        const n = UPDATE_LOG_COUNT.fetchAdd(1, .monotonic);
        if (n < 8) {
            logf("exhaust probe[{d}] emitter=0x{x} index={d} rate={d:.4} pos=({d:.1},{d:.1},{d:.1})", .{ n, emitter, index, rate, readF64At(position), readF64At(position + 8), readF64At(position + 0x10) });
        }
        // One line per distinct effect, with its name, so the log shows
        // exactly which plumes the game is running.
        var first_sighting = false;
        {
            const seen = state.SEEN_INDICES.lock();
            defer state.SEEN_INDICES.unlock();
            if (std.mem.indexOfScalar(u32, seen.items, index) == null and seen.items.len < 32) {
                seen.append(gpa, index) catch {};
                first_sighting = true;
            }
        }
        if (first_sighting) {
            var buf: [80]u8 = undefined;
            const name = sources.readPstr(definition, &buf) orelse "<unreadable>";
            logf("exhaust index {d} first seen: name=\"{s}\" emitter=0x{x} rate={d:.4} pos=({d:.1},{d:.1},{d:.1})", .{ index, name, emitter, rate, readF64At(position), readF64At(position + 8), readF64At(position + 0x10) });
        }
    }

    const temp = sources.exhaustIndexTemperature(index) orelse return;
    if (rate <= offsets.EMITTER_MIN_RATE) {
        // The plume has stopped: drop the source rather than leaving a phantom
        // at the last position it reported.
        const pending = state.EMITTER_PENDING.lock();
        defer state.EMITTER_PENDING.unlock();
        sources.removeEmitter(pending, emitter);
    }
    if (!(rate > offsets.EMITTER_MIN_RATE and positioned)) return;
    const x = readF64At(position);
    const y = readF64At(position + 8);
    const z = readF64At(position + 0x10);
    if (!(x != 0.0 or y != 0.0 or z != 0.0)) return;
    const pending = state.EMITTER_PENDING.lock();
    defer state.EMITTER_PENDING.unlock();
    const entry: state.Emitter = .{ .key = emitter, .seen_ms = state.nowMs(), .x = x, .y = y, .z = z, .activity = rate, .temp_c = temp, .size_m = ENGINE_DEFAULT_SIZE_M };
    for (pending.items) |*e| {
        if (e.key == emitter) {
            e.* = entry;
            return;
        }
    }
    if (pending.items.len < EMITTER_MAX_PENDING) {
        {
            const keys = state.EXHAUST_EMITTERS.lock();
            defer state.EXHAUST_EMITTERS.unlock();
            if (std.mem.indexOfScalar(usize, keys.items, emitter) == null) {
                if (keys.items.len >= 256) _ = keys.orderedRemove(0);
                keys.append(gpa, emitter) catch {};
            }
        }
        pending.append(gpa, entry) catch {};
    }
}

/// Sanity cap on a radiator's +0x240 temperature (coolant boils well below it).
const FLUID_MAX_PLAUSIBLE_C: f64 = 150.0;

/// Definition (comp+0x58): type u32 at +0x2AC and water_component_type u32 at +0xECC
/// (written by tile_definition_parse 0x3168D0 at 0x316AD4 / 0x317CB5).
const DEF_TYPE_OFFSET: usize = 0x2ac;
const DEF_WATER_TYPE_OFFSET: usize = 0xecc;
const DEF_TYPE_FLUID: u32 = 24;
/// water_component_type of radiators/heat sinks (9), liquid-liquid (16), air-air (17)
/// and air-liquid (18) heat exchangers, per rom/data/definitions.
const HEAT_EMITTING_WATER_TYPES = [_]u32{ 9, 16, 17, 18 };

/// Fluid part whose definition is a radiator, heat sink or heat exchanger.
fn isHeatEmittingFluid(component: usize) bool {
    if (!readable(component + 0x58, 8)) return false;
    const def = readUsizeAt(component + 0x58);
    if (def & 7 != 0 or !readable(def + DEF_TYPE_OFFSET, 4) or !readable(def + DEF_WATER_TYPE_OFFSET, 4)) return false;
    return readU32At(def + DEF_TYPE_OFFSET) == DEF_TYPE_FLUID and
        std.mem.indexOfScalar(u32, &HEAT_EMITTING_WATER_TYPES, readU32At(def + DEF_WATER_TYPE_OFFSET)) != null;
}

/// World position of a component, from a tick's world matrix (arg 5) and origin
/// offset (arg 6): the game computes voxel*0.25 - origin and transforms that by
/// the component's 3x4 world matrix. Reproduced here so the IR source sits on
/// the block itself.
fn componentWorldPosition(component: usize, matrix: usize, origin: usize) ?[3]f64 {
    if (!readable(component, 0x24) or !readable(matrix, 0x90) or !readable(origin, 12)) return null;
    const vx: i32 = @bitCast(readU32At(component + 0x18));
    const vy: i32 = @bitCast(readU32At(component + 0x1c));
    const vz: i32 = @bitCast(readU32At(component + 0x20));
    const dx = util.f(vx) * 0.25 - @as(f64, readF32At(origin));
    const dy = util.f(vy) * 0.25 - @as(f64, readF32At(origin + 4));
    const dz = util.f(vz) * 0.25 - @as(f64, readF32At(origin + 8));
    return .{
        readF64At(matrix + 0x18) * dx + readF64At(matrix + 0x38) * dy + readF64At(matrix + 0x58) * dz + readF64At(matrix + 0x78),
        readF64At(matrix + 0x20) * dx + readF64At(matrix + 0x40) * dy + readF64At(matrix + 0x60) * dz + readF64At(matrix + 0x80),
        readF64At(matrix + 0x28) * dx + readF64At(matrix + 0x48) * dy + readF64At(matrix + 0x68) * dz + readF64At(matrix + 0x88),
    };
}

/// Track one engine-family component for the periodic diagnostics.
fn recordEngineSample(key: usize, family: []const u8, power: f64, heat_value: f64, position: ?[3]f64, size_m: f64) u32 {
    const samples = state.ENGINE_SAMPLES.lock();
    defer state.ENGINE_SAMPLES.unlock();
    for (samples.items) |*sample| {
        if (sample.key != key) continue;
        sample.max_power = util.fmax(sample.max_power, power);
        sample.max_heat = util.fmax(sample.max_heat, heat_value);
        sample.calls +|= 1;
        sample.size_m = size_m;
        if (position) |p| {
            sample.x = p[0];
            sample.y = p[1];
            sample.z = p[2];
        }
        return sample.calls;
    }
    if (samples.items.len < 32) {
        const p = position orelse [3]f64{ 0, 0, 0 };
        samples.append(gpa, .{
            .key = key,
            .family = family,
            .max_power = power,
            .max_heat = heat_value,
            .x = p[0],
            .y = p[1],
            .z = p[2],
            .calls = 1,
            .size_m = size_m,
            .progress = 0.0,
            .propellant = 0.0,
            .drain = 0.0,
            .burning = false,
        }) catch {};
    }
    return 1;
}

/// Update the rocket-only fields of a tracked component.
fn recordRocketState(key: usize, progress: f32, propellant: f32, burning: bool) f32 {
    const samples = state.ENGINE_SAMPLES.lock();
    defer state.ENGINE_SAMPLES.unlock();
    for (samples.items) |*sample| {
        if (sample.key != key) continue;
        // Either counter dropping means the motor is consuming propellant.
        const drain = util.fmax(util.fmax(sample.progress - progress, sample.propellant - propellant), 0.0);
        if (drain > sample.drain) sample.drain = drain;
        sample.progress = progress;
        sample.propellant = propellant;
        sample.burning = burning;
        return drain;
    }
    return 0.0;
}

fn printable(byte: u8) bool {
    return byte >= 0x20 and byte < 0x7f;
}

fn hasLetter(bytes: []const u8) bool {
    for (bytes) |b| if (std.ascii.isAlphabetic(b)) return true;
    return false;
}

/// Best-effort part name for a component definition. Part names such as
/// `solid_rocket_nozzle_large` live on the definition, so scanning its first
/// bytes for a pointer to an identifier-like string labels each component.
fn definitionName(definition: usize, out: *[0x300]u8) []const u8 {
    if (definition < 0x10000 or !readable(definition, 0x200)) return "?";
    // Short names live inline in the std::string buffer (SSO), so scan for
    // inline runs as well as following pointers.
    var run_len: usize = 0;
    for (0..0x300) |offset| {
        // Re-verify each chunk: the 0x200-byte check above does not cover this
        // whole 0x300-byte scan, and one dereference past an unmapped page
        // takes the game down with it.
        if (offset % 64 == 0 and !readable(definition + offset, 64)) break;
        const byte = memory.peek(u8, definition + offset);
        if (printable(byte)) {
            out[run_len] = byte;
            run_len += 1;
            continue;
        }
        if (run_len >= 4) {
            const text = out[0..run_len];
            if (hasLetter(text) and std.mem.indexOfScalar(u8, text, '/') == null and std.mem.indexOfScalar(u8, text, '.') == null) {
                return text;
            }
        }
        run_len = 0;
    }
    var offset: usize = 0;
    while (offset < 0x200) : (offset += 8) {
        const pointer = readUsizeAt(definition + offset);
        if (pointer < 0x10000 or !readable(pointer, 48)) continue;
        var len: usize = 0;
        for (0..48) |index| {
            const byte = memory.peek(u8, pointer + index);
            if (byte == 0) break;
            if (!printable(byte)) {
                len = 0;
                break;
            }
            out[len] = byte;
            len += 1;
        }
        const bytes = out[0..len];
        if (len < 4 or std.mem.indexOfScalar(u8, bytes, '_') == null) continue;
        // The definition holds both part names ("Modular Engine Cylinder 1x1")
        // and asset paths ("audio/fx/engine_medium_louder.ogg"), and the latter
        // contains an underscore too, so require a printable, letter-bearing
        // string with no path separator or extension.
        if (!hasLetter(bytes) or std.mem.indexOfScalar(u8, bytes, '/') != null or std.mem.indexOfScalar(u8, bytes, '.') != null) continue;
        return bytes;
    }
    return "?";
}

/// Physical size of a component, from its definition's voxel bounds. The
/// middle extent is the block's diameter for both short parts (solid fuel
/// segments) and long ones (liquid motors), so it is a good plume proxy.
fn componentSizeM(component: usize) f64 {
    const definition = readUsizeAt(component + 0x58);
    if (definition < 0x10000 or !readable(definition, offsets.DEF_VOXEL_MAX + 12)) return ENGINE_DEFAULT_SIZE_M;
    var extents = [3]f64{ 0, 0, 0 };
    for (0..3) |axis| {
        const low_int: i32 = @bitCast(readU32At(definition + offsets.DEF_VOXEL_MIN + axis * 4));
        const high_int: i32 = @bitCast(readU32At(definition + offsets.DEF_VOXEL_MAX + axis * 4));
        const low_float: f64 = readF32At(definition + offsets.DEF_VOXEL_MIN + axis * 4);
        const high_float: f64 = readF32At(definition + offsets.DEF_VOXEL_MAX + axis * 4);
        // The parser may store the bounds as integers or floats; accept
        // whichever encoding gives a sane block extent.
        const int_span = @as(i64, high_int) - low_int + 1;
        const float_span = high_float - low_float + 1.0;
        const voxels: f64 = if (int_span >= 1 and int_span <= 32)
            util.f(int_span)
        else if (std.math.isFinite(high_float - low_float) and float_span >= 1.0 and float_span <= 32.0)
            float_span
        else
            return ENGINE_DEFAULT_SIZE_M;
        extents[axis] = voxels * offsets.VOXEL_M;
    }
    std.mem.sort(f64, &extents, {}, struct {
        fn lessThan(_: void, a: f64, b: f64) bool {
            return a < b;
        }
    }.lessThan);
    return extents[1];
}

/// Every scalar field of a component that could plausibly be a temperature,
/// rate or level. Values in the temperature/rate band (20..200) are grouped
/// separately because that is where the anchors are expected - the preset block
/// read 43.4 C, the player reported 91 C at 49.6 RPS and a 39 C radiator - and
/// an f64 pass covers state stored as a double. Offsets are printed so a field
/// can be identified without knowing it in advance.
fn scalarFields(component: usize, limit: usize, out: *std.ArrayList(u8)) void {
    var hot: std.ArrayList(u8) = .empty;
    defer hot.deinit(gpa);
    var rest: std.ArrayList(u8) = .empty;
    defer rest.deinit(gpa);
    var doubles: std.ArrayList(u8) = .empty;
    defer doubles.deinit(gpa);
    var hot_n: usize = 0;
    var rest_n: usize = 0;
    var dbl_n: usize = 0;
    var offset: usize = 0;
    var chunk_ok = false;
    while (offset + 8 <= 0x900) {
        if (offset % 64 == 0) chunk_ok = readable(component + offset, 64);
        if (!chunk_ok) {
            offset += 64;
            continue;
        }
        const as_float = readF32At(component + offset);
        const as_int = readU32At(component + offset);
        const as_double = readF64At(component + offset);
        const finite = std.math.isFinite(as_float);
        if (finite and as_float >= 20.0 and as_float <= 200.0) {
            if (hot_n < limit) {
                hot.print(gpa, " {x}:{d:.4}", .{ offset, as_float }) catch {};
                hot_n += 1;
            }
        } else if (finite and @abs(as_float) < 0.5 and as_int >= 1 and as_int <= 200) {
            if (hot_n < limit) {
                hot.print(gpa, " {x}:i{d}", .{ offset, as_int }) catch {};
                hot_n += 1;
            }
        } else if (finite and as_float >= 0.5 and as_float <= 1500.0) {
            if (rest_n < limit) {
                rest.print(gpa, " {x}:{d:.4}", .{ offset, as_float }) catch {};
                rest_n += 1;
            }
        }
        if (std.math.isFinite(as_double) and as_double >= 20.0 and as_double <= 200.0) {
            if (dbl_n < limit) {
                doubles.print(gpa, " {x}:d{d:.4}", .{ offset, as_double }) catch {};
                dbl_n += 1;
            }
        }
        offset += 4;
    }
    out.print(gpa, " hot:{s} rest:{s} dbl:{s}", .{ hot.items, rest.items, doubles.items }) catch {};
}

fn containsFamily(families: []const []const u8, family: []const u8) bool {
    for (families) |f| if (std.mem.eql(u8, f, family)) return true;
    return false;
}

/// Periodic per-part dump for the families whose state is still being
/// identified, deduplicated by definition pointer so one line per part type
/// shows how each candidate field evolves while the engine runs and cools.
fn dumpClassFields(samples: []const state.EngineSample, families: []const []const u8, label: []const u8) void {
    if (state.DEEP_DUMP_DISABLED.load(.monotonic)) return;
    var seen: std.ArrayList(usize) = .empty;
    defer seen.deinit(gpa);
    var printed: usize = 0;
    for (samples) |sample| {
        if (!containsFamily(families, sample.family)) continue;
        // A sample key can outlive its component (despawn), so the
        // readability guard must come before ANY dereference.
        if (!readable(sample.key, 0x40)) continue;
        const definition = readUsizeAt(sample.key + 0x58);
        if (std.mem.indexOfScalar(usize, seen.items, definition) != null) continue;
        seen.append(gpa, definition) catch {};
        if (printed >= 20) continue;
        printed += 1;
        var name_buf: [0x300]u8 = undefined;
        var fields: std.ArrayList(u8) = .empty;
        defer fields.deinit(gpa);
        scalarFields(sample.key, 60, &fields);
        logf("{s} field[{s} 0x{x} {s} size={d:.2}m vt=0x{x} pos=({d:.1},{d:.1},{d:.1})]{s}", .{
            label,
            sample.family,
            definition,
            definitionName(definition, &name_buf),
            sample.size_m,
            readUsizeAt(sample.key),
            sample.x,
            sample.y,
            sample.z,
            fields.items,
        });
    }
}

fn floatWindow(component: usize, base: usize, count: usize, out: *std.ArrayList(u8)) []const u8 {
    for (0..count) |index| {
        const offset = base + index * 4;
        // Callers verify only the component header, so every read re-verifies
        // its own few bytes rather than trusting the caller's range.
        if (!readable(component + offset, 4)) continue;
        const value = readF32At(component + offset);
        if (value != 0.0 and std.math.isFinite(value) and @abs(value) > 1e-9) {
            out.print(gpa, " {x}:{d:.4}", .{ offset, value }) catch {};
        }
    }
    return out.items;
}

/// Publish one engine block's IR source (or retract it when the engine stops),
/// keyed by the component so each block has exactly one live source.
fn publishEngineSource(key: usize, position: ?[3]f64, power: f64, temperature: f64, size_m: f64) void {
    if (power <= ENGINE_MIN_POWER or temperature <= 0.0) {
        const pending = state.EMITTER_PENDING.lock();
        defer state.EMITTER_PENDING.unlock();
        sources.removeEmitter(pending, key);
        return;
    }
    const p = position orelse return;
    if (p[0] == 0.0 and p[1] == 0.0 and p[2] == 0.0) return;
    const pending = state.EMITTER_PENDING.lock();
    defer state.EMITTER_PENDING.unlock();
    const entry: state.Emitter = .{ .key = key, .seen_ms = state.nowMs(), .x = p[0], .y = p[1], .z = p[2], .activity = power, .temp_c = temperature, .size_m = size_m };
    for (pending.items) |*e| {
        if (e.key == key) {
            e.* = entry;
            return;
        }
    }
    if (pending.items.len < EMITTER_MAX_PENDING) pending.append(gpa, entry) catch {};
}

/// Logged before any filtering, so "hook never fired" can be told apart from
/// "hook fired but the component was rejected".
var HOOK_CALL_NOTES: Atomic(usize) = .init(0);

fn noteHookCall(family: []const u8, component: usize) void {
    if (!verbose) return;
    // Shared budget rather than a per-hook counter: the counter only advances
    // for accepted components, so a rejected one logged on every call and
    // produced a 17 MB log.
    if (HOOK_CALL_NOTES.fetchAdd(1, .monotonic) < 40) {
        logf("{s} hook call this=0x{x}", .{ family, component });
    }
}

/// Same idea for the rejection diagnostics: a bounded number of lines.
var REJECTION_NOTES: Atomic(usize) = .init(0);

fn noteRejection(comptime fmt: []const u8, args: anytype) void {
    if (!verbose) return;
    if (REJECTION_NOTES.fetchAdd(1, .monotonic) < 20) logf(fmt, args);
}

/// Bounded, definition-deduplicated gate for the per-call component probes.
///
/// These were gated on the per-component call count returned by
/// `recordEngineSample`, which holds at most 32 entries: once the table was
/// full, every further component reported "call 1" forever, so the probes
/// logged on every tick - 17 MB in a few minutes, and past 100 MB in a long
/// session, which crashed the game. A probe now fires once per part
/// definition and at most PROBE_DEFS times overall, which is what these probes
/// are actually for: identifying part types, not counting calls.
var PROBE_SEEN: @import("win32.zig").Mutex(std.ArrayList(usize)) = .{ .value = .empty };

fn probeOnce(component: usize) bool {
    const PROBE_DEFS: usize = 24;
    const definition = readUsizeAt(component + 0x58);
    const seen = PROBE_SEEN.lock();
    defer PROBE_SEEN.unlock();
    if (std.mem.indexOfScalar(usize, seen.items, definition) != null or seen.items.len >= PROBE_DEFS) return false;
    seen.append(gpa, definition) catch return false;
    return true;
}

/// A component pointer is only trusted when its vtable points into the image;
/// some call sites pass something else entirely (they showed up as vt=0x0
/// samples with garbage fields).
pub fn looksLikeComponent(component: usize) bool {
    if (component < 0x10000) return false;
    const vtable = readUsizeAt(component);
    // Compare against the module base captured at bootstrap. The first version
    // of this check wrote the image base as 0x1400_0000 (0x14000000, one hex
    // digit short of 0x1_4000_0000) and therefore rejected every real component.
    const base = state.MODULE_BASE.load(.seq_cst);
    return base != 0 and vtable >= base and vtable < base + 0x1000_0000;
}

fn plausiblePosition(position: ?[3]f64) ?[3]f64 {
    const p = position orelse return null;
    for (p) |v| {
        if (!std.math.isFinite(v) or @abs(v) >= 1.0e6) return null;
    }
    if (p[0] == 0.0 and p[1] == 0.0 and p[2] == 0.0) return null;
    return p;
}

/// Shared probe body for the component classes whose heat state is still being
/// identified (the two diesel engine families and the fluid/heat classes).
/// Nothing is published from here - it records the component (size, vtable and
/// world position from the tick's matrix arguments) and dumps every sane scalar
/// field so the offset carrying load or temperature can be spotted without
/// knowing it in advance.
fn classTickProbe(component: usize, calls: *Atomic(usize), family: []const u8, matrix: usize, origin: usize) void {
    _ = calls.fetchAdd(1, .monotonic);
    // Diagnostics only (samples feed the opt-in deep dump): every pipe and tank
    // runs this per tick, so skip the reads, lock and position math unless the
    // deep dump is enabled.
    if (state.DEEP_DUMP_DISABLED.load(.monotonic)) return;
    if (!readable(component, 0x40)) {
        noteRejection("{s} component not readable this=0x{x}", .{ family, component });
        return;
    }
    if (!looksLikeComponent(component)) {
        noteRejection("{s} component rejected this=0x{x} vt=0x{x}", .{ family, component, readUsizeAt(component) });
        return;
    }
    // Same virtual signature as the jet and rocket ticks, so argument 5 is the
    // world matrix and argument 6 the origin offset. Both are readability
    // checked, so a wrong guess yields null rather than a fault.
    const position = plausiblePosition(componentWorldPosition(component, matrix, origin));
    const size_m = componentSizeM(component);
    _ = recordEngineSample(component, family, 0.0, 0.0, position, size_m);
    // The per-definition field dump lives in dumpClassFields (deduplicated
    // and periodic); printing it here too made each first tick ~2 KB.
    if (verbose and probeOnce(component)) {
        const definition = readUsizeAt(component + 0x58);
        var name_buf: [0x300]u8 = undefined;
        logf("{s} tick[0x{x}] vt=0x{x} def=0x{x} name={s} size={d:.2}m pos={f} matrix=0x{x} origin=0x{x}", .{
            family,
            component,
            readUsizeAt(component),
            definition,
            definitionName(definition, &name_buf),
            size_m,
            util.OptPos{ .v = position },
            matrix,
            origin,
        });
    }
}

/// Preset (prebuilt) diesel engine block tick. Unlike the jets and rockets the
/// block really does store a temperature, so its IR source uses that value
/// rather than a synthetic curve.
fn irPresetTickHook(
    component: usize, p2: usize, p3: usize, p4: usize, p5: usize, p6: usize, p7: usize, p8: usize,
    p9: usize, p10: usize, p11: usize, p12: usize, p13: usize, p14: usize, p15: usize, p16: usize,
    p17: usize, p18: usize, p19: usize, p20: usize, p21: usize, p22: usize, p23: usize, p24: usize,
) callconv(.c) usize {
    noteHookCall("preset", component);
    classTickProbe(component, &state.PRESET_TICK_CALLS, "preset", p5, p6);
    if (readable(component, offsets.PRESET_TEMP_OFFSET + 4) and looksLikeComponent(component)) {
        const temperature: f64 = readF32At(component + offsets.PRESET_TEMP_OFFSET);
        const size_m = componentSizeM(component);
        const position = componentWorldPosition(component, p5, p6);
        // A cold block retracts its source; a running one emits at the
        // temperature the game itself reports.
        const power: f64 = if (temperature >= offsets.PRESET_ENGINE_MIN_C) 1.0 else 0.0;
        publishEngineSource(component, position, power, temperature, size_m);
    }
    const original = state.ORIG_PRESET_TICK.load(.seq_cst);
    if (original == 0) return 0;
    const f: *const fn (
        usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize,
        usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize,
    ) callconv(.c) usize = @ptrFromInt(original);
    // 24 args (the tail is read by the callee; steam uses a23 as a container) - forward all.
    return f(component, p2, p3, p4, p5, p6, p7, p8, p9, p10, p11, p12, p13, p14, p15, p16, p17, p18, p19, p20, p21, p22, p23, p24);
}

/// Modular (custom) engine tick, same shape as the preset one.
fn irModularTickHook(
    component: usize, p2: usize, p3: usize, p4: usize, p5: usize, p6: usize, p7: usize, p8: usize,
    p9: usize, p10: usize, p11: usize, p12: usize, p13: usize, p14: usize, p15: usize, p16: usize,
) callconv(.c) usize {
    noteHookCall("custom", component);
    classTickProbe(component, &state.MODULAR_TICK_CALLS, "custom", p5, p6);
    if (readable(component, offsets.MODULAR_TEMP_OFFSET + 4) and looksLikeComponent(component)) {
        const temperature: f64 = readF32At(component + offsets.MODULAR_TEMP_OFFSET);
        const size_m = componentSizeM(component);
        const position = componentWorldPosition(component, p5, p6);
        // Only cylinder parts carry this field; a cold engine retracts.
        const power: f64 = if (temperature >= offsets.ENGINE_MIN_TEMP_C) 1.0 else 0.0;
        publishEngineSource(component, position, power, temperature, size_m);
    }
    const original = state.ORIG_MODULAR_TICK.load(.seq_cst);
    if (original == 0) return 0;
    const f: *const fn (
        usize, usize, usize, usize, usize, usize, usize, usize,
        usize, usize, usize, usize, usize, usize, usize, usize,
    ) callconv(.c) usize = @ptrFromInt(original);
    return f(component, p2, p3, p4, p5, p6, p7, p8, p9, p10, p11, p12, p13, p14, p15, p16);
}

/// Water-class tick: radiators, heat exchangers, tanks, valves and other
/// fluid machinery all share it, so this probe identifies which part types are
/// in the class and which field holds their temperature.
fn irWaterTickHook(
    component: usize, p2: usize, p3: usize, p4: usize, p5: usize, p6: usize, p7: usize, p8: usize,
    p9: usize, p10: usize, p11: usize, p12: usize, p13: usize, p14: usize, p15: usize, p16: usize,
    p17: usize, p18: usize, p19: usize, p20: usize, p21: usize, p22: usize, p23: usize, p24: usize,
) callconv(.c) f64 {
    noteHookCall("fluid", component);
    classTickProbe(component, &state.WATER_TICK_CALLS, "fluid", p5, p6);
    // Only radiators/heat sinks/heat exchangers emit: +0x240 read 145-320 C on cold fuel
    // tanks, so it is not a temperature on other fluid parts. Picked by definition data
    // (type 24 + water_component_type), so modded parts that copy those attributes count.
    if (readable(component, offsets.FLUID_TEMP_OFFSET + 4) and looksLikeComponent(component) and isHeatEmittingFluid(component)) {
        const temperature: f64 = readF32At(component + offsets.FLUID_TEMP_OFFSET);
        const power: f64 = if (temperature >= offsets.ENGINE_MIN_TEMP_C and temperature <= FLUID_MAX_PLAUSIBLE_C) 1.0 else 0.0;
        const size_m = componentSizeM(component);
        const position = componentWorldPosition(component, p5, p6);
        publishEngineSource(component, position, power, temperature, size_m);
    }
    const original = state.ORIG_WATER_TICK.load(.seq_cst);
    if (original == 0) return 0.0;
    const f: *const fn (
        usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize,
        usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize,
    ) callconv(.c) f64 = @ptrFromInt(original);
    // 24 args (the tail is read by the callee; steam uses a23 as a container) - forward all.
    return f(component, p2, p3, p4, p5, p6, p7, p8, p9, p10, p11, p12, p13, p14, p15, p16, p17, p18, p19, p20, p21, p22, p23, p24);
}

/// Steam-class tick (boilers, steam turbines, whistles).
fn irSteamTickHook(
    component: usize, p2: usize, p3: usize, p4: usize, p5: usize, p6: usize, p7: usize, p8: usize,
    p9: usize, p10: usize, p11: usize, p12: usize, p13: usize, p14: usize, p15: usize, p16: usize,
    p17: usize, p18: usize, p19: usize, p20: usize, p21: usize, p22: usize, p23: usize, p24: usize,
) callconv(.c) f64 {
    noteHookCall("steam", component);
    classTickProbe(component, &state.STEAM_TICK_CALLS, "steam", p5, p6);
    const original = state.ORIG_STEAM_TICK.load(.seq_cst);
    if (original == 0) return 0.0;
    const f: *const fn (
        usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize,
        usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize,
    ) callconv(.c) f64 = @ptrFromInt(original);
    // 24 args (the tail is read by the callee; steam uses a23 as a container) - forward all.
    return f(component, p2, p3, p4, p5, p6, p7, p8, p9, p10, p11, p12, p13, p14, p15, p16, p17, p18, p19, p20, p21, p22, p23, p24);
}

var ROCKET_LOG_COUNT: Atomic(usize) = .init(0);
var ROCKET_ACTIVE_LOG_COUNT: Atomic(usize) = .init(0);

/// Rocket-family component tick (solid and liquid motors). Takes sixteen
/// arguments for the same reason as the jet tick.
fn irRocketTickHook(
    component: usize, p2: usize, p3: usize, p4: usize, matrix: usize, origin: usize, p7: usize, p8: usize,
    p9: usize, p10: usize, p11: usize, p12: usize, p13: usize, p14: usize, scene: usize, p16: usize,
) callconv(.c) void {
    rocketTick(component, matrix, origin);
    const original = state.ORIG_ROCKET_TICK.load(.seq_cst);
    if (original == 0) return;
    const f: *const fn (
        usize, usize, usize, usize, usize, usize, usize, usize,
        usize, usize, usize, usize, usize, usize, usize, usize,
    ) callconv(.c) void = @ptrFromInt(original);
    f(component, p2, p3, p4, matrix, origin, p7, p8, p9, p10, p11, p12, p13, p14, scene, p16);
}

fn rocketTick(component: usize, matrix: usize, origin: usize) void {
    if (!(readable(component, 0x860) and looksLikeComponent(component))) return;
    // The tick only runs its burn block when the byte at +0x1d0 is set;
    // otherwise +0x778 keeps its previous value, so an idle rocket must
    // not keep publishing an IR source from a stale reading.
    const burning = readU32At(component + 0x1d0) & 0xff != 0;
    const raw_power: f64 = readF32At(component + 0x778);
    const position = componentWorldPosition(component, matrix, origin);
    const size_m = componentSizeM(component);
    // Solid and liquid motors share one exhaust temperature; only the
    // plume size differs. A motor that consumes fuel but reports no burn
    // fraction (solid boosters) still counts as burning.
    const drain = recordRocketState(component, readF32At(component + 0x468), readF32At(component + 0x7a4), burning);
    const power: f64 = if (raw_power > ENGINE_MIN_POWER) raw_power else if (drain > 0.0) offsets.ROCKET_DRAIN_ACTIVITY else 0.0;
    _ = state.ROCKET_TICK_CALLS.fetchAdd(1, .monotonic);
    _ = recordEngineSample(component, "rocket", power, 0.0, position, size_m);
    if (verbose and probeOnce(component)) {
        const definition = readUsizeAt(component + 0x58);
        const subtype = readU32At(definition + 0xed4);
        logf("rocket probe[0x{x}] power={d:.4} burning={d} def=0x{x} sub={d} flags={d} pos={f}", .{
            component, raw_power, @intFromBool(burning), definition, subtype, readU32At(component + 0x8), util.OptPos{ .v = position },
        });
    }
    if (raw_power > 0.0) _ = state.ROCKET_MAX_POWER_BITS.fetchMax(@bitCast(raw_power), .monotonic);
    if (verbose) {
        // First calls overall, plus the first calls that actually show a
        // running engine - otherwise the probes are eaten by the idle
        // frames before the player has switched anything on.
        var n = ROCKET_LOG_COUNT.fetchAdd(1, .monotonic);
        const active = power > ENGINE_MIN_POWER;
        n = if (n < 12) n else if (active) 100 + ROCKET_ACTIVE_LOG_COUNT.fetchAdd(1, .monotonic) else std.math.maxInt(usize);
        if (n < 12 or (n >= 100 and n < 112)) {
            logf("rocket tick[{d}] component=0x{x} power={d:.4} thrust=({d:.1},{d:.1},{d:.1}) pos={f}", .{
                n,                                 component,                         power,
                readF32At(component + 0x848),      readF32At(component + 0x84c),      readF32At(component + 0x850),
                util.OptPos{ .v = position },
            });
        }
    }
    const clamped = util.fclamp(power, 0.0, 1.0);
    publishEngineSource(component, position, clamped, offsets.ROCKET_T_MIN_C + (offsets.ROCKET_T_MAX_C - offsets.ROCKET_T_MIN_C) * clamped, size_m);
}

var JET_LOG_COUNT: Atomic(usize) = .init(0);
var JET_ACTIVE_LOG_COUNT: Atomic(usize) = .init(0);

/// Jet-family component tick.
///
/// The tick takes SIXTEEN arguments: Ghidra's `in_stack_00000078` and
/// `in_stack_00000080` are arguments 15 and 16, and the jet tick uses arg 15 as
/// the scene/effects manager (`scene + 0x1178`, `+ 0x680`, `+ 0x1238`). A detour
/// that forwards fewer leaves the missing slots holding its own stack garbage,
/// which the original then dereferences - that crashed the game on the first
/// call after the diagnostics had already logged.
fn irJetTickHook(
    component: usize, p2: usize, p3: usize, p4: usize, matrix: usize, origin: usize, p7: usize, p8: usize,
    p9: usize, p10: usize, p11: usize, p12: usize, p13: usize, p14: usize, scene: usize, p16: usize,
) callconv(.c) void {
    jetTick(component, matrix, origin);
    const original = state.ORIG_JET_TICK.load(.seq_cst);
    if (original == 0) return;
    const f: *const fn (
        usize, usize, usize, usize, usize, usize, usize, usize,
        usize, usize, usize, usize, usize, usize, usize, usize,
    ) callconv(.c) void = @ptrFromInt(original);
    f(component, p2, p3, p4, matrix, origin, p7, p8, p9, p10, p11, p12, p13, p14, scene, p16);
}

fn jetTick(component: usize, matrix: usize, origin: usize) void {
    if (!(readable(component, 0x1dc) and looksLikeComponent(component))) return;
    // +0x1bc is the spool speed: every turbine adds to it and the
    // extracted power is proportional to it.
    const power: f64 = readF32At(component + offsets.JET_RPS_OFFSET);
    const exhaust_c: f64 = readF32At(component + 0x1cc);
    const heat_value: f64 = readF32At(component + 0x1d8);
    const field_d0: f64 = readF32At(component + 0x1d0);
    const definition = readUsizeAt(component + 0x58);
    const part_type: u32 = if (definition >= 0x10000) readU32At(definition + offsets.DEF_JET_PART_TYPE) & 0xff else 0;
    const position = componentWorldPosition(component, matrix, origin);
    _ = state.JET_TICK_CALLS.fetchAdd(1, .monotonic);
    const size_m = componentSizeM(component);
    _ = recordEngineSample(component, "jet", power, heat_value, position, size_m);
    if (power > 0.0) _ = state.JET_MAX_POWER_BITS.fetchMax(@bitCast(power), .monotonic);
    if (heat_value > 0.0) _ = state.JET_MAX_HEAT_BITS.fetchMax(@bitCast(heat_value), .monotonic);
    if (verbose) {
        var n = JET_LOG_COUNT.fetchAdd(1, .monotonic);
        const active = heat_value > ENGINE_MIN_POWER or power > ENGINE_MIN_POWER;
        n = if (n < 12) n else if (active) 100 + JET_ACTIVE_LOG_COUNT.fetchAdd(1, .monotonic) else std.math.maxInt(usize);
        if (n < 12 or (n >= 100 and n < 112)) {
            logf("jet tick[{d}] component=0x{x} power={d:.4} exhaust_c={d:.1} heat={d:.4} pos={f}", .{ n, component, power, exhaust_c, heat_value, util.OptPos{ .v = position } });
        }
    }
    // The game has no Celsius gas temperature anywhere: +0x1cc/+0x1d0
    // are enthalpy (fuel * RPS) in two scales, which is how a compressor
    // could read 2382 C at high spool. So the gas path is synthesised
    // per part role. Below JET_RPS_TYPICAL the whole engine scales down
    // toward ambient rather than sitting at running temperatures.
    const is_exhaust = part_type == offsets.JET_EXHAUST_PART_TYPE;
    const ab_on = is_exhaust and (readU32At(component + offsets.JET_AFTERBURNER_OFFSET) & 0xff) != 0;
    // The exhaust part never accumulates spool, so its operating point
    // comes from its 0..1 flow field instead of its own RPS.
    const eff_rps = if (is_exhaust)
        util.fclamp(heat_value, 0.0, offsets.JET_EXHAUST_FLOW_FULL) / offsets.JET_EXHAUST_FLOW_FULL * offsets.JET_RPS_FULL
    else
        power;
    const running = if (is_exhaust) eff_rps > offsets.JET_RPS_TYPICAL * 0.01 else power > ENGINE_MIN_POWER;
    var t_low: f64 = offsets.JET_COMPRESSOR_T_C;
    var t_high: f64 = offsets.JET_COMPRESSOR_T_C;
    if (is_exhaust) {
        if (ab_on) {
            t_low = offsets.JET_AB_T_LOW;
            t_high = offsets.JET_AB_T_HIGH;
        } else {
            t_low = offsets.JET_EXHAUST_T_LOW;
            t_high = offsets.JET_EXHAUST_T_HIGH;
        }
    } else if (part_type == 2 or part_type == 3) {
        t_low = offsets.JET_CASING_T_LOW;
        t_high = offsets.JET_CASING_T_HIGH;
    }
    const ir_temp_c: f64 = if (!running)
        0.0
    else if (eff_rps < offsets.JET_RPS_TYPICAL)
        t_low * (eff_rps / offsets.JET_RPS_TYPICAL)
    else blk: {
        const blend = util.fclamp((eff_rps - offsets.JET_RPS_TYPICAL) / (offsets.JET_RPS_FULL - offsets.JET_RPS_TYPICAL), 0.0, 1.0);
        break :blk t_low + (t_high - t_low) * blend;
    };
    const activity = if (is_exhaust)
        util.fclamp(heat_value / offsets.JET_EXHAUST_FLOW_FULL, 0.0, 1.0)
    else
        util.fclamp(power / offsets.JET_RPS_FULL, 0.0, 1.0);
    {
        const parts = state.JET_PARTS.lock();
        defer state.JET_PARTS.unlock();
        // A despawned part never ticks again, so its stamp stopped
        // advancing; drop it here and the heartbeat stops listing jets
        // that no longer exist.
        const seen = state.nowMs();
        var keep: usize = 0;
        for (parts.items) |e| {
            if (seen -| e.seen_ms < state.EMITTER_STALE_MS) {
                parts.items[keep] = e;
                keep += 1;
            }
        }
        parts.shrinkRetainingCapacity(keep);
        const entry: state.JetPart = .{
            .component = component,
            .definition = definition,
            .part_type = part_type,
            .afterburner = @intFromBool(ab_on),
            .rps = power,
            .heat = heat_value,
            .exhaust = exhaust_c,
            .d0 = field_d0,
            .size_m = size_m,
            .ir_temp_c = ir_temp_c,
            .seen_ms = seen,
        };
        for (parts.items) |*slot| {
            if (slot.component == component) {
                slot.* = entry;
                break;
            }
        } else if (parts.items.len < 48) {
            parts.append(gpa, entry) catch {};
        }
    }
    publishEngineSource(component, position, activity, ir_temp_c, size_m);
}

/// One-shot verbose key: `hooks` owns bits 22..32 of the shared word
/// (`blocktemp` owns 0..8, `flir` 8..22).
const LOG_TEMPERATURE_PROVIDER: u32 = 1 << 22;

/// True when `p7` — argument 7 of the temperature evaluator, the vehicle
/// air-volume provider — is a game object the evaluator can call.
///
/// The original DOES NOT check it: after the heat/wind loops it does
/// `mov rcx,[provider]` / `mov rax,[rcx]` / `call [rax+0x28]` at 0x30FFE1..0x31001A
/// with no test, so any non-object (the observed bug is a stray 32-bit `-1`
/// landing in the provider slot, giving `0xFFFFFFFFFFFFFFFF`) faults the game
/// thread inside the call. Both the vtable and the called slot must be game
/// image memory, or the call through them is not ours to make.
///
/// Deliberately cheap (this hook runs for every temperature probe): one region
/// query on the object, then the image-range checks. `inGameImage` proves
/// committed image memory, so the vtable's `+0x28` read needs no own check.
inline fn temperatureProviderOk(p7: usize) bool {
    if (!memory.inPtrRange(p7)) return false;
    if (!readable(p7, 8)) return false;
    const vtable = readUsizeAt(p7);
    if (!memory.inGameImage(vtable)) return false;
    return memory.inGameImage(readUsizeAt(vtable + 0x28));
}

/// The value to report when `temperatureProviderOk` rejects `p7`: the air
/// volume's own temperature (the leaf the provider would hand back), else the
/// ambient base at `p1`, else the fixed fallback. `p9` is usually 0 and both
/// pointers are game-owned, so every dereference stays guarded.
fn temperatureFallback(p1: usize, p9: usize) f32 {
    if (p9 >= PTR_MIN and readable(p9 + 0x74, 4)) {
        const air = readF32At(p9 + 0x74);
        if (std.math.isFinite(air)) return air;
    }
    if (p1 >= PTR_MIN and readable(p1, 24)) {
        const pos = [3]f64{ readF64At(p1), readF64At(p1 + 8), readF64At(p1 + 16) };
        if (std.math.isFinite(pos[0]) and std.math.isFinite(pos[1]) and std.math.isFinite(pos[2])) {
            return heat.baseAmbientAt(pos);
        }
    }
    return heat.WORLD_FALLBACK_C;
}

var TEMP_DUMP_COUNT: Atomic(usize) = .init(0);

fn irTemperatureHook(p1: usize, p2: usize, p3: usize, p4: usize, p5: usize, p6: usize, p7: usize, p8: usize, p9: usize) callconv(.c) f32 {
    heat.cacheVariant(&.{ p2, p3, p4, p5, p6, p7, p8, p9 });
    state.TEMP_READY.store(true, .seq_cst);
    if (!state.TEMP_FIRST.swap(true, .seq_cst)) {
        logLine("temperature hook first call: heat containers cached");
    }

    // Periodic container dump independent of ir.scan: engines and temperature
    // sensors drive this hook even when no seeker component exists, so the
    // diagnostics stay alive in sessions without one.
    if (verbose) temperatureDump(p2, p5);

    const original = state.ORIG_TEMPERATURE.load(.seq_cst);
    if (original == 0) return 0.0;
    // p7 is dereferenced unconditionally by the original (0x31000C), so an
    // invalid provider must be rejected before the call, never forwarded.
    if (!temperatureProviderOk(p7)) {
        logging.logOnce(LOG_TEMPERATURE_PROVIDER, "temperature hook: invalid provider p7=0x{x}; returning the ambient fallback instead of calling the evaluator", .{p7});
        return temperatureFallback(p1, p9);
    }
    const f: *const fn (usize, usize, usize, usize, usize, usize, usize, usize, usize) callconv(.c) f32 = @ptrFromInt(original);
    return f(p1, p2, p3, p4, p5, p6, p7, p8, p9);
}

fn temperatureDump(p2: usize, p5: usize) void {
    const n = TEMP_DUMP_COUNT.fetchAdd(1, .monotonic);
    if (n % 600 != 0) return;
    logf("temperature hook dump #{d}: heat=0x{x} fires=0x{x} same={}", .{ n, p2, p5, p2 == p5 });
    heat.logHeatContainer(p2);
    heat.logVolumeHeatSources(p2);
    heat.logFiresContainer(p5);
    const jet_calls = state.JET_TICK_CALLS.swap(0, .monotonic);
    const rocket_calls = state.ROCKET_TICK_CALLS.swap(0, .monotonic);
    const jet_power: f64 = @bitCast(state.JET_MAX_POWER_BITS.swap(0, .monotonic));
    const jet_heat: f64 = @bitCast(state.JET_MAX_HEAT_BITS.swap(0, .monotonic));
    const rocket_power: f64 = @bitCast(state.ROCKET_MAX_POWER_BITS.swap(0, .monotonic));
    const engine_sources = blk: {
        const pending = state.EMITTER_PENDING.lock();
        defer state.EMITTER_PENDING.unlock();
        break :blk pending.items.len;
    };
    logf("engine state: jet_calls={d} jet_max_power={d:.4} jet_max_heat={d:.4}     rocket_calls={d} rocket_max_power={d:.4} ir_sources={d}", .{ jet_calls, jet_power, jet_heat, rocket_calls, rocket_power, engine_sources });

    const samples = state.ENGINE_SAMPLES.lock();
    defer state.ENGINE_SAMPLES.unlock();
    // Reads stale component/definition pointers unchecked: deep-dump only.
    if (state.DEEP_DUMP_DISABLED.load(.monotonic)) samples.clearRetainingCapacity();
    std.mem.sort(state.EngineSample, samples.items, {}, struct {
        fn lessThan(_: void, a: state.EngineSample, b: state.EngineSample) bool {
            return util.fmax(a.max_power, a.max_heat) > util.fmax(b.max_power, b.max_heat);
        }
    }.lessThan);
    logf("engine blocks tracked: {d}", .{samples.items.len});
    for (samples.items[0..@min(samples.items.len, 10)]) |sample| {
        var name_buf: [0x300]u8 = undefined;
        logf("  {s} 0x{x} name={s} size={d:.2}m max_power={d:.4} max_heat={d:.4} burning={d}     drain={d:.5} prop={d:.4} fuel={d:.4} segments={d:.1} pos=({d:.1},{d:.1},{d:.1})", .{
            sample.family,
            sample.key,
            definitionName(readUsizeAt(sample.key + 0x58), &name_buf),
            sample.size_m,
            sample.max_power,
            sample.max_heat,
            @intFromBool(sample.burning),
            sample.drain,
            sample.propellant,
            readF32At(sample.key + 0x414),
            readF32At(sample.key + 0x1f0),
            sample.x,
            sample.y,
            sample.z,
        });
    }
    // Live per-component field windows, so a rocket that only shows state
    // while burning (solid boosters) is captured too.
    var rockets: usize = 0;
    for (samples.items) |sample| {
        if (!std.mem.eql(u8, sample.family, "rocket")) continue;
        if (rockets >= 10) break;
        rockets += 1;
        if (!readable(sample.key, 0x790)) continue;
        const definition = readUsizeAt(sample.key + 0x58);
        var w1: std.ArrayList(u8) = .empty;
        defer w1.deinit(gpa);
        var w2: std.ArrayList(u8) = .empty;
        defer w2.deinit(gpa);
        var w3: std.ArrayList(u8) = .empty;
        defer w3.deinit(gpa);
        var w4: std.ArrayList(u8) = .empty;
        defer w4.deinit(gpa);
        logf("rocket fields[0x{x}] def=0x{x} sub={d} throttle={d:.4} win1c0:{s} win270:{s}     win400:{s} win760:{s}", .{
            sample.key,
            definition,
            readU32At(definition + 0xed4),
            readF32At(sample.key + 0x268),
            floatWindow(sample.key, 0x1c0, 8, &w1),
            floatWindow(sample.key, 0x270, 12, &w2),
            floatWindow(sample.key, 0x400, 40, &w3),
            floatWindow(sample.key, 0x760, 20, &w4),
        });
    }
    dumpClassFields(samples.items, &.{ "preset", "custom", "fluid", "steam" }, "class");
    samples.clearRetainingCapacity();
}

var FIRES_FIRST: Atomic(bool) = .init(false);

/// Cache the fires-system holder from the fire vertex builder (its arg 1 owns
/// the fires ring at +0x11d8..+0x11e8, the same one the temperature function
/// reads as its fires argument). This makes flares and burning fires visible
/// even before any temperature evaluation (engine/probe) has run this session.
fn irFiresHook(p1: usize, p2: usize, p3: usize, p4: usize, p5: usize, p6: usize) callconv(.c) void {
    if (p1 >= 0x10000) {
        state.FIRES_HOLDER.store(p1, .monotonic);
        if (!FIRES_FIRST.swap(true, .monotonic)) logLine("fires builder hook first call: fires holder cached");
    }
    const original = state.ORIG_FIRES_BUILD.load(.seq_cst);
    if (original == 0) return;
    const f: *const fn (usize, usize, usize, usize, usize, usize) callconv(.c) void = @ptrFromInt(original);
    f(p1, p2, p3, p4, p5, p6);
}

/// Hook on the game's per-tick sun update FUN_1405e3270: run the original
/// first (it writes the sun sprite position), then capture that position.
///
/// ABI note: the day percent arrives in XMM1 (the callee saves XMM1 and
/// feeds it to FUN_1402bf9e0), so the real signature carries another
/// float-class parameter ahead of it that the decompiler lost. Declaring
/// TWO float parameters keeps both XMM0 and XMM1 round-tripped unchanged
/// under either reading, so the trampoline always sees the exact incoming
/// register state; whichever float is in 0..1 is the day percent.
fn irSunUpdateHook(p1: usize, f0: f32, f1: f32, p3: u32, p4: usize) callconv(.c) void {
    const original = state.ORIG_SUN_UPDATE.load(.seq_cst);
    if (original != 0) {
        const f: *const fn (usize, f32, f32, u32, usize) callconv(.c) void = @ptrFromInt(original);
        f(p1, f0, f1, p3, p4);
    }
    if (p4 < 0x10000) return;
    const x = readF64At(p4 + 0x198);
    const y = readF64At(p4 + 0x1a0);
    const z = readF64At(p4 + 0x1a8);
    const n = @sqrt(x * x + y * y + z * z);
    if (!(n > 100.0 and n < 1.0e9)) return;
    {
        const g = state.SUN_DIR.lock();
        defer state.SUN_DIR.unlock();
        g.* = .{ x, y, z };
    }
    const pct_f: f32 = if (f1 >= 0.0 and f1 <= 1.0) f1 else if (f0 >= 0.0 and f0 <= 1.0) f0 else 0.0;
    state.SUN_PCT.store(util.f2i(usize, util.fclamp(@as(f64, pct_f) * 1000.0, 0.0, 1_000_000.0)), .monotonic);
    if (!state.SUN_DIR_VALID.swap(true, .monotonic)) {
        logf("sun update hook first call: sprite=({d:.0},{d:.0},{d:.0}) f0={d:.4} f1={d:.4}", .{ x, y, z, f0, f1 });
    }
}

var SCENE_FROM_UNIFORMS_LOGGED: Atomic(bool) = .init(false);

pub const FrameUniformsFn = *const fn (usize, usize, usize, usize, usize, u32, i8, u8, u8, usize, usize, u8, u8, u8, usize) callconv(.c) void;

/// Hook on the per-frame render uniform fill FUN_1406bb400.  It runs on the
/// render thread every frame (unlike the sim-side scene updates, which may
/// only run at world load), and its arg 2 IS the scene object — the very
/// same object whose clock subsystem it touches at +0xbaeb8.  Capturing the
/// scene here makes the day clock and sky state available no matter
/// when the plugin was injected.
///
/// ABI: 15 integer-class arguments.  All are round-tripped unchanged.
pub fn irFrameUniformsHook(
    p1: usize, p2: usize, p3: usize, p4: usize, p5: usize, p6: u32, p7: i8, p8: u8,
    p9: u8, p10: usize, p11: usize, p12: u8, p13: u8, p14: u8, p15: usize,
) callconv(.c) void {
    // FLIR: this is the scene submit, so the per-view IR state must be ready
    // BEFORE the pass draws anything. The mesh -> body map is NOT built here:
    // this runs on the render thread and the game rebuilds the vehicle/body
    // containers on its own thread, so the walk lives in the scene tick and
    // only the published map is read here.
    if (p11 >= 0x10000) flir.onSceneSubmit(p2, p11, p12);

    const original = state.ORIG_FRAME_UNIFORMS.load(.seq_cst);
    if (original != 0) {
        const f: FrameUniformsFn = @ptrFromInt(original);
        f(p1, p2, p3, p4, p5, p6, p7, p8, p9, p10, p11, p12, p13, p14, p15);
    }
    if (p2 >= 0x10000) {
        const prev = state.SCENE.swap(p2, .monotonic);
        if (prev != p2 and !SCENE_FROM_UNIFORMS_LOGGED.swap(true, .monotonic)) {
            logf("scene cached from render uniforms: scene=0x{x} clock={d}", .{ p2, readU32At(p2 + 0xbaec0) });
        }
    }
    if (p11 >= 0x10000) {
        const base = p11 + 0x8f0; // +0x11e doubles
        const x = readF64At(base);
        const y = readF64At(base + 8);
        const z = readF64At(base + 16);
        const n = @sqrt(x * x + y * y + z * z);
        if (n > 1.0e-6 and n < 1.0e9) {
            {
                const g = state.LIGHT_DIR.lock();
                defer state.LIGHT_DIR.unlock();
                g.* = .{ x, y, z };
            }
            state.LIGHT_DIR_VALID.store(true, .monotonic);
        }
    }
}

var SCENE_TICK_FIRST: Atomic(bool) = .init(false);

/// Cache the scene object from the scene tick (its arg 1 IS the scene): the
/// day-cycle clock (+0xbaec0, copied from the authoritative +0xbbaac each
/// tick) and the sky state all live on it. Runs every game tick.
fn irSceneTickHook(p1: usize, p2: usize, p3: usize, p4: usize, p5: usize, p6: usize, p7: usize, p8: usize, p9: usize, p10: usize) callconv(.c) usize {
    // One synchronous pass on the game thread (our sim plus the game's vehicle
    // update with its per-component tick hooks): readable() results are reused
    // within it instead of one VirtualQuery per component per hook call.
    const regions = memory.RegionCacheScope.begin();
    defer regions.end();
    if (p1 >= 0x10000) {
        const previous = state.SCENE.swap(p1, .monotonic);
        if (previous != 0 and previous != p1) {
            // New world (load / host / rejoin): pointers cached from the old
            // scene are freed memory that may still read as valid.
            heat.forgetWorld();
            state.FIRES_HOLDER.store(0, .monotonic);
            flir.RENDER_TARGETS_STALE.store(true, .monotonic);
            logf("scene changed 0x{x} -> 0x{x}: world caches dropped", .{ previous, p1 });
        }
        if (!SCENE_TICK_FIRST.swap(true, .monotonic)) logLine("scene tick hook first call: scene object cached (sun live)");
        // scene tick: drive the FLIR block temperature simulation and cache the
        // ambient temperature at the IR camera (game thread, so the heat model is
        // only ever evaluated from here). The sim has its own kill switch: it is
        // the phase most likely to touch a freshly spawned vehicle's containers.
        if (!state.SIM_DISABLED.load(.monotonic)) blocktemp.tick(p1);
        flir.sceneTick(p1);
    }
    const original = state.ORIG_SCENE_TICK.load(.seq_cst);
    if (original == 0) return 0;
    // 10 args (a7 is a float in its stack slot, forwarded bit-exact); returns __int64.
    const f: *const fn (usize, usize, usize, usize, usize, usize, usize, usize, usize, usize) callconv(.c) usize = @ptrFromInt(original);
    return f(p1, p2, p3, p4, p5, p6, p7, p8, p9, p10);
}

// ---------------------------------------------------------------------------
// installation
// ---------------------------------------------------------------------------

pub const InstallError = struct {
    buf: [512]u8 = undefined,
    len: usize = 0,

    pub fn set(self: *InstallError, comptime fmt: []const u8, args: anytype) error{HookInstall} {
        const text = std.fmt.bufPrint(&self.buf, fmt, args) catch self.buf[0..];
        self.len = text.len;
        return error.HookInstall;
    }

    pub fn message(self: *const InstallError) []const u8 {
        return self.buf[0..self.len];
    }
};

/// Verify `prologue` at `address` or fail naming `what`.
fn gate(err: *InstallError, address: usize, prologue: []const u8, comptime what: []const u8) error{HookInstall}!void {
    if (!memory.bytesMatch(address, prologue)) {
        return err.set(what ++ " prologue mismatch at 0x{x}; this build is not {s}", .{ address, offsets.GAME_BUILD_LABEL });
    }
}

/// Create and enable a detour on `target`, storing the trampoline in `orig`.
fn hook(err: *InstallError, target: usize, detour: *const anyopaque, orig: *Atomic(usize), comptime what: []const u8) error{HookInstall}!void {
    var status: minhook.Status = undefined;
    const original = minhook.createHook(target, detour, &status) orelse
        return err.set("MinHook create_hook(" ++ what ++ ") failed: {f}", .{status});
    orig.store(original, .seq_cst);
    if (!minhook.enableHook(target, &status)) {
        return err.set("MinHook enable_hook(" ++ what ++ ") failed: {f}", .{status});
    }
}

pub fn installHooks(err: *InstallError) error{HookInstall}!void {
    const base = state.MODULE_BASE.load(.seq_cst);
    if (base == 0) return err.set("module base is not resolved", .{});

    const registrar = base + offsets.RVA_COMPONENT_LUA_REGISTRAR;
    try gate(err, registrar, &offsets.REGISTRAR_PROLOGUE, "component Lua registrar");
    const temperature = base + offsets.RVA_TEMPERATURE;
    try gate(err, temperature, &offsets.TEMPERATURE_PROLOGUE, "temperature function");
    const fires_build = base + offsets.RVA_FIRES_BUILD;
    try gate(err, fires_build, &offsets.FIRES_PROLOGUE, "fire vertex builder");
    const scene_tick = base + offsets.RVA_SCENE_TICK;
    try gate(err, scene_tick, &offsets.SCENE_TICK_PROLOGUE, "scene tick");
    const sun_update = base + offsets.RVA_SUN_UPDATE;
    try gate(err, sun_update, &offsets.SUN_UPDATE_PROLOGUE, "sun update");
    const frame_uniforms = base + offsets.RVA_FRAME_UNIFORMS;
    try gate(err, frame_uniforms, &offsets.FRAME_UNIFORMS_PROLOGUE, "frame uniforms");

    // The drawing environment's initializer. Deliberately non-fatal: if this
    // prologue ever mismatches, the seeker and every other hook must still
    // install, and only the on-screen image is lost.
    const screen_init = base + offsets.RVA_LUA_SCREEN_INIT;
    if (!memory.bytesMatch(screen_init, &offsets.SCREEN_INIT_PROLOGUE)) {
        logf("screen Lua init prologue mismatch at 0x{x}; ir.frame will not reach microcontroller scripts", .{screen_init});
    } else if (state.SCREEN_HOOKS_DISABLED.load(.seq_cst)) {
        logLine("screen Lua init hook disabled by request");
    } else {
        var status: minhook.Status = undefined;
        if (minhook.createHook(screen_init, &irScreenInitHook, &status)) |original| {
            state.ORIG_SCREEN_INIT.store(original, .seq_cst);
            if (!minhook.enableHook(screen_init, &status)) logf("MinHook enable_hook(screen Lua init) failed: {f}", .{status});
        } else {
            logf("MinHook create_hook(screen Lua init) failed: {f}", .{status});
        }
    }

    if (state.EXHAUST_HOOKS_DISABLED.load(.seq_cst)) return;

    try hook(err, registrar, &irRegistrarHook, &state.ORIG_REGISTRAR, "registrar");
    try hook(err, temperature, &irTemperatureHook, &state.ORIG_TEMPERATURE, "temperature");
    try hook(err, fires_build, &irFiresHook, &state.ORIG_FIRES_BUILD, "fires builder");
    try hook(err, scene_tick, &irSceneTickHook, &state.ORIG_SCENE_TICK, "scene tick");
    try hook(err, sun_update, &irSunUpdateHook, &state.ORIG_SUN_UPDATE, "sun update");
    try hook(err, frame_uniforms, flir.sceneSubmitEntry(), &state.ORIG_FRAME_UNIFORMS, "frame uniforms");

    if (!state.ROCKET_HOOKS_DISABLED.load(.seq_cst)) {
        const rocket_tick = base + offsets.RVA_ROCKET_TICK;
        try gate(err, rocket_tick, &offsets.ROCKET_TICK_PROLOGUE, "rocket tick");
        try hook(err, rocket_tick, &irRocketTickHook, &state.ORIG_ROCKET_TICK, "rocket tick");
    }

    if (!state.JET_HOOKS_DISABLED.load(.seq_cst)) {
        const jet_tick = base + offsets.RVA_JET_TICK;
        try gate(err, jet_tick, &offsets.JET_TICK_PROLOGUE, "jet tick");
        try hook(err, jet_tick, &irJetTickHook, &state.ORIG_JET_TICK, "jet tick");
    }

    if (!state.PRESET_HOOKS_DISABLED.load(.seq_cst)) {
        const preset_tick = base + offsets.RVA_PRESET_ENGINE_TICK;
        try gate(err, preset_tick, &offsets.PRESET_ENGINE_TICK_PROLOGUE, "preset engine tick");
        try hook(err, preset_tick, &irPresetTickHook, &state.ORIG_PRESET_TICK, "preset engine tick");
    }

    if (!state.MODULAR_HOOKS_DISABLED.load(.seq_cst)) {
        const modular_tick = base + offsets.RVA_MODULAR_ENGINE_TICK;
        try gate(err, modular_tick, &offsets.MODULAR_ENGINE_TICK_PROLOGUE, "modular engine tick");
        try hook(err, modular_tick, &irModularTickHook, &state.ORIG_MODULAR_TICK, "modular engine tick");
    }

    if (!state.FLUID_HOOKS_DISABLED.load(.seq_cst)) {
        const water_tick = base + offsets.RVA_WATER_TICK;
        try gate(err, water_tick, &offsets.WATER_TICK_PROLOGUE, "water tick");
        try hook(err, water_tick, &irWaterTickHook, &state.ORIG_WATER_TICK, "water tick");

        const steam_tick = base + offsets.RVA_STEAM_TICK;
        try gate(err, steam_tick, &offsets.STEAM_TICK_PROLOGUE, "steam tick");
        try hook(err, steam_tick, &irSteamTickHook, &state.ORIG_STEAM_TICK, "steam tick");
    }

    if (!state.EXHAUST_HOOKS_DISABLED.load(.seq_cst)) {
        const exhaust_update = base + offsets.RVA_EXHAUST_UPDATE;
        try gate(err, exhaust_update, &offsets.EXHAUST_UPDATE_PROLOGUE, "exhaust update");
        try hook(err, exhaust_update, &irExhaustUpdateHook, &state.ORIG_EXHAUST_UPDATE, "exhaust update");
    }

    // FLIR render side: tonemap replacement, block pass and camera IR latch.
    // Every piece is independent and fail-closed; only this feature is lost on
    // a mismatch.
    flir.install(base);

    state.HOOKS_INSTALLED.store(true, .seq_cst);
}
