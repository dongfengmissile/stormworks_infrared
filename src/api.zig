//! The Lua surface the components call: `ir.probe` and `ir.scan` in the
//! component-mod state, `ir.frame` / `ir.flir` / `ir.flir_config` in the microcontroller
//! (screen) state, plus the table registration the two initializers invoke.

const std = @import("std");
const blocktemp = @import("blocktemp.zig");
const heat = @import("heat.zig");
const hooks = @import("hooks.zig");
const ir = @import("ir.zig");
const logging = @import("logging.zig");
const lua_api = @import("lua.zig");
const memory = @import("memory.zig");
const offsets = @import("offsets.zig");
const sources = @import("sources.zig");
const state = @import("state.zig");
const sun = @import("sun.zig");
const util = @import("util.zig");
const win = @import("win32.zig");
const fx = @import("fx.zig");

const Atomic = std.atomic.Value;
const gpa = state.gpa;
const verbose = logging.verbose;
const logLine = logging.logLine;
const logf = logging.logf;
const readable = memory.readable;
const readUsizeAt = memory.readUsizeAt;
const readU32At = memory.readU32At;
const readF32At = memory.readF32At;
const TAU = offsets.TAU;
const FnPair = lua_api.FnPair;

const luaArgNumber = lua_api.luaArgNumber;
const luaArgBool = lua_api.luaArgBool;
const luaPushNumber = lua_api.luaPushNumber;
const luaPushBoolean = lua_api.luaPushBoolean;
const luaPushTable = lua_api.luaPushTable;
const luaRawseti = lua_api.luaRawseti;

const StringAllocFn = *const fn (*usize, u32) callconv(.c) void;
const StringFillFn = *const fn (usize, [*:0]const u8, u32) callconv(.c) void;
const RegisterTableFn = *const fn (usize, *const usize, [*]const FnPair, usize) callconv(.c) void;

/// Register the `ir` global into the component-mod Lua state, exactly the way
/// the game registers its own `matrix` / `parser` / `component` tables.
pub fn registerIrTable(context: usize) void {
    if (context == 0) return;
    const alloc = memory.gameFn(StringAllocFn, offsets.RVA_STRING_ALLOC);
    const fill = memory.gameFn(StringFillFn, offsets.RVA_STRING_FILL);
    const register = memory.gameFn(RegisterTableFn, offsets.RVA_REGISTER_TABLE);

    var name_object: usize = 0;
    alloc(&name_object, 3);
    if (name_object == 0) {
        logLine("ir registration: name allocation failed");
        return;
    }
    fill(name_object, "ir", 3);

    const pairs = [_]FnPair{
        .{ .name = "scan", .func = @intFromPtr(&irScan) },
        .{ .name = "probe", .func = @intFromPtr(&irProbe) },
        .{ .name = null, .func = 0 },
    };

    register(context, &name_object, &pairs, context + 0x28);
}

/// Register the `ir` table into the drawing (screen) Lua environment.
///
/// `ir.frame` reads the image a seeker published, `ir.probe` samples the heat
/// model, and `ir.flir_config` resolves this microcontroller's video link to
/// the camera it feeds - none of them needs a component-handle upvalue.
///
/// `ir.scan` is deliberately absent: it is a seeker API (a component with a
/// world matrix and a physics context for occlusion), and in this state the
/// game's upvalues are per-table script objects (`screen`, `async`, ...), not
/// component handles (RE_NOTES §1.7.1). Registering it here previously handed
/// `luaComponent` the address of this function's own local, which is neither a
/// component nor valid after the initializer returned.
///
/// The upvalue is therefore NULL, the same value the game hands its own
/// `debug` table, so `luaComponent` / `luaComponentHandle` return 0 instead
/// of reading through a dangling pointer. A microcontroller call that needs its
/// component derives it from the `L-8` back-pointer instead (`mcComponent`).
pub fn registerIrScreenTable(lua_state: usize) void {
    if (lua_state == 0) return;
    const alloc = memory.gameFn(StringAllocFn, offsets.RVA_STRING_ALLOC);
    const fill = memory.gameFn(StringFillFn, offsets.RVA_STRING_FILL);
    const register = memory.gameFn(RegisterTableFn, offsets.RVA_REGISTER_TABLE);

    var name_object: usize = 0;
    alloc(&name_object, 3);
    if (name_object == 0) {
        logLine("ir screen registration: name allocation failed");
        return;
    }
    fill(name_object, "ir", 3);

    const pairs = [_]FnPair{
        .{ .name = "frame", .func = @intFromPtr(&irFrame) },
        .{ .name = "probe", .func = @intFromPtr(&irProbe) },
        .{ .name = "flir_config", .func = @intFromPtr(&irFlirConfig) },
        .{ .name = "flir", .func = @intFromPtr(&irFlir) },
        .{ .name = null, .func = 0 },
    };

    // The fourth argument is the lightuserdata the game installs as the one
    // upvalue of every registered function. NULL is what the game itself hands
    // its `debug` table, and nothing above dereferences it.
    register(lua_state, &name_object, &pairs, 0);
}

// ---------------------------------------------------------------------------
// microcontroller component lookup (RE_NOTES §1.7.1-1.7.3)
// ---------------------------------------------------------------------------

/// `c_scene_vehicle_component_camera` vtable.
const CAMERA_VTABLE_RVA: usize = 0x00b5_3420;
/// `c_vehicle_logic_slot_output_video::vftable` - a camera's video output slot.
const SLOT_OUTPUT_VIDEO_VTABLE_RVA: usize = 0x00b7_46f8;
/// The microcontroller's embedded `c_microprocessor_lua_script`.
const MC_SCRIPT_OFFSET: usize = 0x660;
/// A camera's `c_vehicle_logic_slot_output_video` (camera + 0x190).
const CAMERA_VIDEO_SLOT: usize = 0x190;

/// Why a microcontroller call could not resolve a component or a camera, for
/// the one-shot log line.
const NO_MC_STATE = "not called from a microcontroller Lua state (no L-8 script back-pointer)";
const NO_CAMERA = "this microcontroller's video input is not fed by a camera video output";

/// `c_microprocessor_lua_script` -> owning microcontroller component.
///
/// The game's screen init writes the script object to `*(u64*)(L-8)`
/// (`0x2E5261`), and the script object is embedded in the component at
/// `+0x660`, so `component = *(u64*)(L-8) - 0x660`. Both hops are validated
/// (readable, and a class vtable inside the game image) before the component is
/// returned, because a Lua state from the component-mod side has no such
/// back-pointer.
fn mcComponent(lua: usize) ?usize {
    if (lua < 0x10000 or !readable(lua - 8, 8)) return null;
    const script = readUsizeAt(lua - 8);
    if (!readable(script, 8) or !memory.inGameImage(readUsizeAt(script))) return null;
    const base = state.MODULE_BASE.load(.seq_cst);
    if (base == 0 or readUsizeAt(script) != base + LUA_SCRIPT_VTABLE_RVA) return null;
    const component = script -% MC_SCRIPT_OFFSET;
    if ((rd(component) orelse return null) != base + LUA_BLOCK_VTABLE_RVA) return null;
    return component;
}

/// Vtable RVAs of the MC graph objects (RE_NOTES §1.7.3).
const LUA_SCRIPT_VTABLE_RVA: usize = 0x00b6_f468; // c_microprocessor_lua_script
const LUA_BLOCK_VTABLE_RVA: usize = 0x00b9_a710; // c_microprocessor_component_lua_script
const OUT_VIDEO_NODE_VTABLE_RVA: usize = 0x00af_72c8; // ..._logic_node_output_video
const BRIDGE_VIDEO_IN_VTABLE_RVA: usize = 0x00b0_72d8; // ..._bridge_video_input
const VIDEO_SWITCHBOX_VTABLE_RVA: usize = 0x00b8_2ee8; // ..._video_switchbox
/// Input node fields: source slot / source (graph) output node.
const NODE_SRC_NODE: usize = 0x30;
/// bridge_video_input's in-node (+0x50); switchbox in-nodes at +0x50 and +0x2A8.
const SWITCHBOX_IN_NODES = [2]usize{ 0x50, 0x2a8 };
/// Switchbox's cached bool switch value (bool in-node +0x500, value +0x38).
const SWITCHBOX_STATE: usize = 0x538;
const MC_VIDEO_IN_NODE: usize = 0x110;
const MC_HOP_CAP: usize = 8;

/// Guarded 8-aligned read.
fn rd(p: usize) ?usize {
    if (p < 0x10000 or p & 7 != 0 or !readable(p, 8)) return null;
    return readUsizeAt(p);
}

const MC_GROUP: usize = 0x9a8;
const GROUP_VTABLE_RVA: usize = 0x00b4_6bd8;
const GROUP_IN_MC: usize = 0x1c0;
const MC_COMP_VTABLE_RVA: usize = 0x00b8_4dc8;
const MC_BODY: usize = 0x28;
const MC_INPUT_SLOT_VTABLE_RVA: usize = 0x00b7_e7e8; // c_vehicle_logic_slot_input_video
const BRIDGE_INSLOT: usize = 0x78;
const BODY_COMPONENTS: usize = 0x3f0; // ring: ptr, +8 cap, +0xC off, +0x10 n
const BODY_VEHICLE: usize = 0x250;
const VEHICLE_BODIES: usize = 0x50; // ring of 0x30-byte entries, body* at +0x18
const SLOT_CONSUMERS: usize = 0x238; // ring: ptr, +8 cap, +0xC off, +0x10 n
const RING_CAP_MAX: usize = 4096;
const SCAN_BUDGET: usize = 20000;

/// Diagnostics of one MC camera resolve.
const McTrace = struct {
    group_vt: ?usize = null,
    mc: usize = 0,
    mc_vt: ?usize = null,
    inslot: usize = 0,
    inslot_vt: ?usize = null,
    body: usize = 0,
    vehicle: usize = 0,
    bodies: usize = 0,
    cameras: usize = 0,
    consumer_counts: std.ArrayList(usize) = .empty,
    hops: std.ArrayList(usize) = .empty,
    result: []const u8 = "",

    fn deinit(self: *McTrace) void {
        self.consumer_counts.deinit(gpa);
        self.hops.deinit(gpa);
    }
};

/// Validated ring header: (ptr, cap, off, n).
const RingHeader = struct { ptr: usize, cap: usize, off: usize, n: usize };

fn ring(p: usize) ?RingHeader {
    const ptr = rd(p) orelse return null;
    const word = rd(p + 8) orelse return null;
    const cap = word & 0xffff_ffff;
    const off = (word >> 32) & 0xffff_ffff;
    const n = (rd(p + 0x10) orelse return null) & 0xffff_ffff;
    if (ptr == 0 or cap == 0 or cap > RING_CAP_MAX or n > cap) return null;
    return .{ .ptr = ptr, .cap = cap, .off = off, .n = n };
}

/// Find the MC's own input port slot by following the input node chain.
fn mcInslot(base: usize, lua_blk: usize, t: *McTrace) ?usize {
    var in_node = lua_blk + MC_VIDEO_IN_NODE;
    for (0..MC_HOP_CAP) |_| {
        const node = rd(in_node + NODE_SRC_NODE) orelse return null;
        const vt = rd(node) orelse return null;
        t.hops.append(gpa, vt -% base) catch {};
        if (vt != base + OUT_VIDEO_NODE_VTABLE_RVA) return null;
        const owner = rd(node + 8) orelse return null;
        const ovt = rd(owner) orelse return null;
        t.hops.append(gpa, ovt -% base) catch {};
        if (ovt == base + BRIDGE_VIDEO_IN_VTABLE_RVA) {
            const slot = rd(owner + BRIDGE_INSLOT) orelse return null;
            t.inslot = slot;
            t.inslot_vt = rd(slot);
            return if (t.inslot_vt == base + MC_INPUT_SLOT_VTABLE_RVA) slot else null;
        } else if (ovt == base + VIDEO_SWITCHBOX_VTABLE_RVA) {
            // Follow the input the switch currently forwards (0x140389DA0:
            // state byte != 0 -> +0x50, else +0x2A8).
            if (!readable(owner + SWITCHBOX_STATE, 1)) return null;
            const on = memory.peek(u8, owner + SWITCHBOX_STATE) != 0;
            in_node = owner + SWITCHBOX_IN_NODES[if (on) 0 else 1];
        } else {
            return null;
        }
    }
    return null;
}

/// Scan one body's components; push cameras into `cams`. Returns the camera
/// whose consumer ring contains `inslot`.
fn scanBody(base: usize, body: usize, inslot: usize, cams: *std.ArrayList(usize), budget: *usize, t: *McTrace) ?usize {
    const r = ring(body + BODY_COMPONENTS) orelse return null;
    var found: ?usize = null;
    for (0..r.n) |i| {
        if (budget.* == 0) break;
        budget.* -= 1;
        const c = rd(r.ptr + 8 * ((i + r.off) % r.cap)) orelse continue;
        if (c == 0 or rd(c) != base + CAMERA_VTABLE_RVA or std.mem.indexOfScalar(usize, cams.items, c) != null) continue;
        cams.append(gpa, c) catch {};
        t.cameras += 1;
        const consumers = ring(c + CAMERA_VIDEO_SLOT + SLOT_CONSUMERS) orelse {
            t.consumer_counts.append(gpa, std.math.maxInt(usize)) catch {};
            continue;
        };
        t.consumer_counts.append(gpa, consumers.n) catch {};
        if (inslot != 0 and found == null) {
            for (0..consumers.n) |j| {
                if (budget.* == 0) break;
                budget.* -= 1;
                if (rd(consumers.ptr + 8 * ((j + consumers.off) % consumers.cap)) == inslot) {
                    found = c;
                    break;
                }
            }
        }
    }
    return found;
}

fn mcResolve(lua_blk: usize, t: *McTrace) ?usize {
    const base = state.MODULE_BASE.load(.seq_cst);
    if (base == 0) return null;
    const group = rd(lua_blk + MC_GROUP) orelse return null;
    t.group_vt = rd(group);
    if (t.group_vt != base + GROUP_VTABLE_RVA) {
        t.result = "group vt";
        return null;
    }
    t.mc = group -% GROUP_IN_MC;
    t.mc_vt = rd(t.mc);
    if (t.mc_vt != base + MC_COMP_VTABLE_RVA) {
        t.result = "mc vt";
        return null;
    }
    const inslot = mcInslot(base, lua_blk, t) orelse 0;
    t.body = rd(t.mc + MC_BODY) orelse return null;
    if (t.body == 0) {
        t.result = "no body";
        return null;
    }
    var cams: std.ArrayList(usize) = .empty;
    defer cams.deinit(gpa);
    var budget: usize = SCAN_BUDGET;
    if (scanBody(base, t.body, inslot, &cams, &budget, t)) |c| {
        t.result = "consumer match (body)";
        return c;
    }
    t.vehicle = rd(t.body + BODY_VEHICLE) orelse 0;
    if (t.vehicle != 0) {
        // Other bodies (subgrids) of the vehicle: the body-pointer set at
        // vehicle+0 that blocktemp walks every tick, then the older 0x30-byte
        // ring at +0x50. A camera on a different subgrid than the MC was
        // never found through the ring alone.
        var others: std.ArrayList(usize) = .empty;
        defer others.deinit(gpa);
        if (blocktemp.readSet(t.vehicle, 256, 8, blocktemp.MAX_SET_MOD)) |set| {
            for (0..set.count()) |j| others.append(gpa, set.item(j)) catch {};
        }
        if (ring(t.vehicle + VEHICLE_BODIES)) |r| {
            for (0..r.n) |i| {
                if (rd(r.ptr + 0x30 * ((i + r.off) % r.cap) + 0x18)) |b| others.append(gpa, b) catch {};
            }
        }
        for (others.items) |b| {
            if (b == 0 or b == t.body or rd(b + BODY_VEHICLE) != t.vehicle) continue;
            t.bodies += 1;
            if (scanBody(base, b, inslot, &cams, &budget, t)) |c| {
                t.result = "consumer match (vehicle)";
                return c;
            }
        }
    }
    if (cams.items.len == 1) {
        t.result = "sole camera fallback";
        return cams.items[0];
    }
    t.result = "no match / ambiguous";
    return null;
}

/// lua block -> (camera, input slot it was matched through, resolve time ms).
const CameraCacheEntry = struct { camera: usize, slot: usize, at_ms: u64 };
var MC_CAMERA_CACHE: win.Mutex(fx.HashMap(usize, CameraCacheEntry)) = .{ .value = .empty };

/// The camera whose video output feeds the MC Lua block `component` (the
/// `lua_blk` from `mcComponent`).
fn mcCamera(component: usize) ?usize {
    const base = state.MODULE_BASE.load(.seq_cst);
    if (base == 0) return null;
    if ((rd(component) orelse return null) != base + LUA_BLOCK_VTABLE_RVA) return null;
    // The full resolve scans every component of the vehicle (two per tick per
    // script: ir.flir + ir.flir_config). Reuse the last answer while the
    // cheap last hop still holds: the camera keeps its vtable and its
    // consumer ring still contains this MC's input slot (re-derived from the
    // live node chain). A sole-camera fallback (no slot) is re-scanned
    // every second.
    const now = state.nowMs();
    var t: McTrace = .{};
    defer t.deinit();
    const inslot = mcInslot(base, component, &t) orelse 0;
    const cached = blk: {
        const cache = MC_CAMERA_CACHE.lock();
        defer MC_CAMERA_CACHE.unlock();
        break :blk cache.get(component);
    };
    if (cached) |entry| {
        const fresh = if (entry.slot != 0)
            entry.slot == inslot and cameraFeeds(base, entry.camera, entry.slot)
        else
            inslot == 0 and now -| entry.at_ms < 1000 and rd(entry.camera) == base + CAMERA_VTABLE_RVA;
        if (fresh) return entry.camera;
    }
    var resolve_trace: McTrace = .{};
    defer resolve_trace.deinit();
    const camera = mcResolve(component, &resolve_trace) orelse return null;
    const cache = MC_CAMERA_CACHE.lock();
    defer MC_CAMERA_CACHE.unlock();
    if (cache.count() >= 256) cache.clearRetainingCapacity();
    cache.put(gpa, component, .{ .camera = camera, .slot = inslot, .at_ms = now }) catch {};
    return camera;
}

/// `camera` is still a camera and its video output's consumer ring holds `slot`.
fn cameraFeeds(base: usize, camera: usize, slot: usize) bool {
    if (rd(camera) != base + CAMERA_VTABLE_RVA) return false;
    const consumers = ring(camera + CAMERA_VIDEO_SLOT + SLOT_CONSUMERS) orelse return false;
    for (0..@min(consumers.n, 64)) |j| {
        if (rd(consumers.ptr + 8 * ((j + consumers.off) % consumers.cap)) == slot) return true;
    }
    return false;
}

var MC_FAILURE_LOGGED: Atomic(bool) = .init(false);

/// One-shot (verbose builds) diagnostics for a failed camera resolve.
fn logMcFailure(lua: usize) void {
    if (!verbose) return;
    if (MC_FAILURE_LOGGED.swap(true, .monotonic)) return;
    const base = state.MODULE_BASE.load(.seq_cst);
    const script = rd(lua -% 8) orelse 0;
    const lua_blk = script -% MC_SCRIPT_OFFSET;
    var t: McTrace = .{};
    defer t.deinit();
    const r = mcResolve(lua_blk, &t);
    const Rva = struct {
        v: ?usize,
        base: usize,
        pub fn format(self: @This(), writer: *std.Io.Writer) std.Io.Writer.Error!void {
            if (self.v) |v| try writer.print("0x{x}", .{v -% self.base}) else try writer.writeAll("unreadable");
        }
    };
    const payload: ?usize = if (readable(lua_blk + 0x360, 4)) readU32At(lua_blk + 0x360) else null;
    logf("mc camera resolve failed: lua_blk=0x{x} result={f} ({s}) group_vt={f} (want 0x{x}) mc=0x{x} vt={f} (want 0x{x}) inslot=0x{x} vt={f} (want 0x{x}) body=0x{x} vehicle=0x{x} other_bodies={d} cameras_scanned={d} consumer_counts={f} hop_vtable_rvas={f} payload_count={f}", .{
        lua_blk,
        util.OptHex{ .v = if (r) |c| c -% base else null },
        t.result,
        Rva{ .v = t.group_vt, .base = base },
        GROUP_VTABLE_RVA,
        t.mc,
        Rva{ .v = t.mc_vt, .base = base },
        MC_COMP_VTABLE_RVA,
        t.inslot,
        Rva{ .v = t.inslot_vt, .base = base },
        MC_INPUT_SLOT_VTABLE_RVA,
        t.body,
        t.vehicle,
        t.bodies,
        t.cameras,
        util.List(usize, false){ .items = t.consumer_counts.items },
        util.List(usize, true){ .items = t.hops.items },
        util.OptHex{ .v = payload },
    });
}

// ---------------------------------------------------------------------------
// injected Lua functions
// ---------------------------------------------------------------------------

pub fn irProbe(lua_ptr: ?*anyopaque) callconv(.c) i32 {
    const lua = @intFromPtr(lua_ptr);
    const x = luaArgNumber(lua, 1) orelse 0.0;
    const y = luaArgNumber(lua, 2) orelse 0.0;
    const z = luaArgNumber(lua, 3) orelse 0.0;
    luaPushNumber(lua, heat.temperatureAt(x, y, z));
    return 1;
}

/// A seeker's last rendered image, for the drawing Lua environment.
///
/// Takes the seeker's id (its `ir.scan` return value, which the seeker
/// publishes to the microcontroller over the array composite), so a vehicle -
/// or a world - with several seekers draws exactly the one it is wired to.
///
/// Returns (cells, width, height), cells holding one 0..255 value per cell in
/// row-major order. This exists because a logic composite is 32 floats: packed
/// two cells per float that caps an image at 64 cells, so a 64x64 frame cannot
/// reach a monitor that way. The drawing environment has no component context
/// either, so it cannot run the scan itself - it reads what the seeker
/// published instead.
pub fn irFrame(lua_ptr: ?*anyopaque) callconv(.c) i32 {
    const lua = @intFromPtr(lua_ptr);
    // arg 1: which seeker's frame. 0 (or omitted) returns nothing, because
    // a seeker's id is never 0 - the drawing script passes what it read from
    // the composite, so it asks for its own seeker and no other.
    const id: u32 = @intCast(std.math.clamp(util.f2i(i64, luaArgNumber(lua, 1) orelse 0.0), 0, offsets.FRAME_ID_MAX));
    var width: i32 = 0;
    var height: i32 = 0;
    var bytes: std.ArrayList(u8) = .empty;
    defer bytes.deinit(gpa);
    {
        const frames = state.LAST_FRAME.lock();
        defer state.LAST_FRAME.unlock();
        if (frames.get(id)) |frame| {
            width = frame.width;
            height = frame.height;
            bytes.appendSlice(gpa, frame.bytes.items) catch {};
        }
    }
    _ = luaPushTable(lua);
    for (bytes.items, 0..) |value, index| {
        luaPushNumber(lua, util.f(value));
        luaRawseti(lua, @as(i64, @intCast(index)) + 1);
    }
    luaPushNumber(lua, util.f(width));
    luaPushNumber(lua, util.f(height));
    return 3;
}

/// A seeker's identity for the frame map: 24 bits derived from its component.
///
/// Live components have distinct pointers, so this is unique among the seekers
/// actually running, without anyone having to number them - which is the point:
/// a channel number is global to the whole game process, so two vehicles that
/// both picked 1 would overwrite each other's image. It changes when a vehicle
/// is respawned, which the drawing side simply re-reads.
pub fn seekerId(component: usize) u32 {
    if (component == 0) return 0;
    // Mix the whole pointer down to 24 bits rather than truncating it, so two
    // components that differ only in their high bits still separate, and the
    // value does not read as a raw address. Deterministic for a given component.
    var mixed: u64 = @as(u64, component) ^ 0x9e37_79b9_7f4a_7c15;
    mixed *%= 0xff51_afd7_ed55_8ccd;
    mixed ^= mixed >> 29;
    const id: u32 = @intCast(mixed & offsets.FRAME_ID_MAX);
    return if (id == 0) 1 else id;
}

// ---------------------------------------------------------------------------
// FLIR camera configuration (`ir.flir_config`, microcontroller state only)
// ---------------------------------------------------------------------------

/// Narrowest display window accepted. A zero (or near-zero) span would divide
/// the shader by ~0 instead of mapping temperatures, so it is rejected as an
/// invalid argument rather than producing an unusable image.
const FLIR_MIN_WINDOW_C: f32 = 0.01;
/// Cap on configured cameras. Keys are component pointers and a long session
/// spawns cameras repeatedly, so the map is bounded; on overflow it is cleared
/// and every live script republishes its config on its next tick.
const FLIR_CONFIG_MAX: usize = 256;

var FLIR_CONFIG_LOGGED: Atomic(bool) = .init(false);

/// `ir.flir_config(r, g, b, black_hot, tmin, tmax)` - microcontroller only.
///
/// Sets the FLIR presentation of the camera whose video output feeds this
/// microcontroller's video input, keyed by the camera component pointer - the
/// key `flir.zig` reads from `FLIR_CONFIG` for the view it is compositing.
/// Returns true when the link resolved and the config was stored, false (and a
/// one-shot log line) otherwise. The link fields are only valid while the
/// game's tick owns them, which is exactly when a script call runs, so they are
/// read here and never cached.
pub fn irFlirConfig(lua_ptr: ?*anyopaque) callconv(.c) i32 {
    const lua = @intFromPtr(lua_ptr);
    var reason: []const u8 = "";
    const config = flirConfigArgs(lua, &reason) orelse return flirConfigFailed(lua, reason);
    const component = mcComponent(lua) orelse return flirConfigFailed(lua, NO_MC_STATE);
    const camera = mcCamera(component) orelse {
        logMcFailure(lua);
        return flirConfigFailed(lua, NO_CAMERA);
    };

    {
        const configs = state.FLIR_CONFIG.lock();
        defer state.FLIR_CONFIG.unlock();
        if (configs.count() >= FLIR_CONFIG_MAX and !configs.contains(camera)) configs.clearRetainingCapacity();
        const entry = configs.getOrPutValue(gpa, camera, .{}) catch null;
        if (entry) |e| {
            var updated = config;
            updated.enabled = e.value_ptr.enabled;
            updated.enabled_ms = e.value_ptr.enabled_ms;
            e.value_ptr.* = updated;
        }
    }

    // One-shot in-game validation of the inferred link walk: the first
    // successful call proves which camera the MC is actually feeding.
    if (!FLIR_CONFIG_LOGGED.swap(true, .monotonic)) {
        logf("ir.flir_config: camera=0x{x} mc=0x{x} source_node=0x0 slot_vt=0x{x} camera_vt=0x{x} cfg=([{d}, {d}, {d}], black_hot={}, window=[{d}, {d}] C)", .{
            camera,
            component,
            readUsizeAt(camera + CAMERA_VIDEO_SLOT),
            readUsizeAt(camera),
            config.rgb[0],
            config.rgb[1],
            config.rgb[2],
            config.black_hot,
            config.tmin,
            config.tmax,
        });
    }
    luaPushBoolean(lua, true);
    return 1;
}

/// Parse and validate the six `ir.flir_config` arguments.
fn flirConfigArgs(lua: usize, reason: *[]const u8) ?state.FlirCfg {
    var rgb = [3]f32{ 0, 0, 0 };
    for (&rgb, 0..) |*slot, index| {
        const value = luaArgNumber(lua, @as(i32, @intCast(index)) + 1) orelse {
            reason.* = "r, g and b must be numbers (0..1 tint)";
            return null;
        };
        if (!std.math.isFinite(value)) {
            reason.* = "r, g and b must be finite numbers";
            return null;
        }
        slot.* = @floatCast(std.math.clamp(value, 0.0, 1.0));
    }
    const black_hot = luaArgBool(lua, 4) orelse {
        reason.* = "black_hot must be a boolean or a number";
        return null;
    };
    const tmin_arg = luaArgNumber(lua, 5);
    const tmax_arg = luaArgNumber(lua, 6);
    if (tmin_arg == null or tmax_arg == null) {
        reason.* = "tmin and tmax must be numbers (Celsius)";
        return null;
    }
    if (!std.math.isFinite(tmin_arg.?) or !std.math.isFinite(tmax_arg.?)) {
        reason.* = "tmin and tmax must be finite numbers";
        return null;
    }
    // Convert first: an f64 beyond the f32 range becomes infinite, which the
    // shader would then receive as a window edge.
    const tmin: f32 = @floatCast(tmin_arg.?);
    const tmax: f32 = @floatCast(tmax_arg.?);
    if (!std.math.isFinite(tmin) or !std.math.isFinite(tmax) or !(tmin < tmax)) {
        reason.* = "tmin must be below tmax";
        return null;
    }
    if (tmax - tmin < FLIR_MIN_WINDOW_C) {
        reason.* = "the display window must be at least 0.01 C wide";
        return null;
    }
    return .{ .rgb = rgb, .black_hot = black_hot, .tmin = tmin, .tmax = tmax };
}

var FLIR_CONFIG_FAILED_LOGGED: Atomic(bool) = .init(false);

/// Push `false` for a failed config call and log the reason once, so a script
/// calling this every tick cannot flood the log before the camera is wired.
fn flirConfigFailed(lua: usize, reason: []const u8) i32 {
    if (!FLIR_CONFIG_FAILED_LOGGED.swap(true, .monotonic)) {
        logf("ir.flir_config returned false: {s} (only the first failure is logged)", .{reason});
    }
    luaPushBoolean(lua, false);
    return 1;
}

/// `ir.flir(on)` - microcontroller only. Enables FLIR for the camera feeding
/// this MC's video input; must be called every tick (expires after 1 s).
/// `false`/`0` disables immediately. Returns true if a camera was resolved.
pub fn irFlir(lua_ptr: ?*anyopaque) callconv(.c) i32 {
    const lua = @intFromPtr(lua_ptr);
    const resolved: ?struct { on: bool, camera: usize } = blk: {
        const on = luaArgBool(lua, 1) orelse break :blk null;
        const component = mcComponent(lua) orelse break :blk null;
        const camera = mcCamera(component) orelse {
            logMcFailure(lua);
            break :blk null;
        };
        break :blk .{ .on = on, .camera = camera };
    };
    const r = resolved orelse {
        luaPushBoolean(lua, false);
        return 1;
    };
    {
        const configs = state.FLIR_CONFIG.lock();
        defer state.FLIR_CONFIG.unlock();
        if (configs.count() >= FLIR_CONFIG_MAX and !configs.contains(r.camera)) configs.clearRetainingCapacity();
        if (configs.getOrPutValue(gpa, r.camera, .{}) catch null) |entry| {
            entry.value_ptr.enabled = r.on;
            entry.value_ptr.enabled_ms = state.nowMs();
        }
    }
    luaPushBoolean(lua, true);
    return 1;
}

// ---------------------------------------------------------------------------
// ir.scan
// ---------------------------------------------------------------------------

/// Signed azimuth/elevation offset of a source from the steering centre, and
/// its cone distance (the azimuth leg is scaled by cos(elevation)).
const ConeOffset = struct { daz: f64, del: f64, off: f64 };

fn coneOffset(s: *const ir.IrSource, az_center: f64, el_center: f64) ConeOffset {
    const d = s.az - az_center;
    const daz = d - TAU * util.round(d / TAU);
    const del = s.el - el_center;
    const a = daz * util.fmax(@cos(s.el), 0.2);
    return .{ .daz = daz, .del = del, .off = @sqrt(a * a + del * del) };
}

var CONE_LOGGED: Atomic(bool) = .init(false);
var TRACE_LINES: Atomic(u32) = .init(0);
var LAST_STEP_LOG: Atomic(u64) = .init(0);
var LAST_ERR: win.Mutex(std.AutoHashMapUnmanaged(u32, [2]f64)) = .{ .value = .empty };

/// One heat source as seen by the IR sensor.
pub fn irScan(lua_ptr: ?*anyopaque) callconv(.c) i32 {
    const t0 = if (verbose) win.elapsedNs() else 0;
    const result = scanInner(@intFromPtr(lua_ptr));
    if (verbose) blocktemp.SCAN_COST.add(t0);
    return result;
}

fn scanInner(lua: usize) i32 {
    // Synchronous on the game thread: reuse readable() results within the scan
    // (a no-op when the scene-tick scope is already active around the script).
    const regions = memory.RegionCacheScope.begin();
    defer regions.end();

    // Azimuth steering is clamped to the same 0.25-turn deflection the
    // elevation axis has by construction, so a steering command cannot
    // swing the scan cone past the gimbal limit.
    const az_steer = util.fclamp(luaArgNumber(lua, 1) orelse 0.0, -offsets.STEER_LIMIT_TURNS, offsets.STEER_LIMIT_TURNS);
    const el_steer = luaArgNumber(lua, 2) orelse 0.0;
    const fov = luaArgNumber(lua, 3) orelse 0.0833;
    const range = luaArgNumber(lua, 4) orelse 200.0;
    const res_x: i32 = std.math.clamp(util.f2i(i32, luaArgNumber(lua, 5) orelse 8.0), 1, 64);
    const res_y: i32 = std.math.clamp(util.f2i(i32, luaArgNumber(lua, 6) orelse 4.0), 1, 64);
    // Arg 7 = IR gain: flux that maps to full-scale white.
    // Arg 8 = detect threshold (raw flux); the Lua applies it.
    const gain = luaArgNumber(lua, 7) orelse 0.1;
    _ = luaArgNumber(lua, 8) orelse 0.01;
    const axis = util.f2i(i32, luaArgNumber(lua, 9) orelse 4.0);
    const occlude = (luaArgNumber(lua, 10) orelse 1.0) != 0.0;
    // arg 11: pack the image 2 cells per float (12-bit each) when the grid
    // is larger than one composite channel set (33..64 cells).
    const pack = (luaArgNumber(lua, 11) orelse 0.0) != 0.0;
    // arg 12: whether to return the cell table. A seeker whose image reaches
    // the microcontroller through ir.frame (rather than a composite) can skip
    // it: at 64x64 that is 4096 Lua table insertions per scan for values
    // nobody reads. An empty table is still returned, to keep the order of
    // the other return values.
    const cells_out = (luaArgNumber(lua, 12) orelse 1.0) != 0.0;

    const cell_count: usize = @intCast(res_x * res_y);
    var cells_buf: [64 * 64]f64 = undefined;
    const cells = cells_buf[0..cell_count];
    @memset(cells, 0.0);
    var az_error: f64 = 0.0;
    var el_error: f64 = 0.0;
    var strength: f64 = 0.0;
    var total_flux: f64 = 0.0;
    var base_temp: f64 = 20.0;
    var source_count: usize = 0;

    // Diagnostics-only scan state for the throttled verbose log.
    var diag_fwd: ir.Vec3 = .{ 0, 0, 0 };
    var diag_visible: std.ArrayList([5]f64) = .empty;
    defer diag_visible.deinit(gpa);
    var diag_fov: f64 = 0.0;
    var diag_cone: f64 = 0.0;
    var diag_steer = [2]f64{ 0, 0 };
    // Normalised centroid (wx/ws, wy/ws, weight_sum) for the throttled
    // dump - lets a pinned-at-centre error be told apart from a real one.
    var diag_centroid = [3]f64{ 0, 0, 0 };
    var packed_vals: std.ArrayList(f64) = .empty;
    defer packed_vals.deinit(gpa);

    const component = lua_api.luaComponent(lua);
    const seeker_id = seekerId(component);
    var origin: ir.Vec3 = .{ 0, 0, 0 };
    // The scan runs whenever the component exists; without a temperature
    // evaluation yet (no engine/probe this session) heat sources and the
    // real ambient are unavailable, but fires/flares still are (fires
    // holder cached by the fire-render hook).
    // A foreign Lua state can hand back a pointer that is not a component at
    // all, so the vtable is checked before anything is read through it.
    if (component != 0 and hooks.looksLikeComponent(component)) {
        const matrix = ir.readComponentMatrix(component);
        origin = .{ matrix[12], matrix[13], matrix[14] };
        const axes = ir.axesFor(axis, &matrix);
        const forward = ir.normalize(axes[0]);
        const up = axes[1];
        // right = up x fwd. With the game's handedness this is the seeker's
        // physical right for the +Y-forward mesh mount (verified in game:
        // the other order mirrors the array image left-right).
        const right = ir.normalize(ir.cross(up, forward));

        const fov_rad = fov * TAU;
        const az_center = az_steer * TAU;
        const el_center = el_steer * TAU;

        // One line per plugin load: proves which cone geometry is running
        // and records the configured FOV, so "is the game executing the
        // new build" is answerable from the player build's bootstrap log.
        if (!CONE_LOGGED.swap(true, .monotonic)) {
            logf("scan armed: fov={d:.4} turns ({d:.2} deg full, {d:.2} deg half-angle), \nerror sources: in-cone centroid only", .{ fov, util.degrees(fov_rad), util.degrees(fov_rad * 0.5) });
        }

        var weight_sum: f64 = 0.0;
        var weight_x: f64 = 0.0;
        var weight_y: f64 = 0.0;
        const az_sp = fov_rad / util.f(res_x);
        const el_sp = fov_rad / util.f(res_y);
        const cell_solid = az_sp * el_sp;

        // Collect every heat source and its emitted IR. This is radiometry
        // (emission ~ T^4 * area, received ~ 1/d^2), not the game's local
        // temperature field, so distant hot sources stay visible.
        var list: std.ArrayList(ir.IrSource) = .empty;
        defer list.deinit(gpa);
        const vcount = @min(state.VARIANT_COUNT.load(.monotonic), state.MAX_VARIANTS);
        var fires_holders = [_]usize{0} ** (state.MAX_VARIANTS + 1);
        var fires_n: usize = 0;
        // shortcut: the air at the seeker stands in for the air at each
        // source and for the scene background; per-source air if it matters.
        base_temp = heat.baseAmbientAt(origin);
        for (0..vcount) |i| {
            const a = heat.variantArgs(i);
            sources.collectIrSources(a[0], &origin, range, base_temp, &list);
            sources.collectVolumeHeatSources(a[0], &origin, range, base_temp, &list);
            const h = a[3];
            if (h >= 0x10000 and std.mem.indexOfScalar(usize, fires_holders[0..fires_n], h) == null) {
                fires_holders[fires_n] = h;
                fires_n += 1;
            }
        }
        const fh = state.FIRES_HOLDER.load(.monotonic);
        if (fh >= 0x10000 and std.mem.indexOfScalar(usize, fires_holders[0..fires_n], fh) == null) {
            fires_holders[fires_n] = fh;
            fires_n += 1;
        }
        for (fires_holders[0..fires_n]) |h| sources.collectFireSources(h, &origin, range, &list);
        sources.collectEmitterSources(&origin, range, &list);
        // A seeker sees contrast: only radiance above the background's, so
        // something at or below the air temperature is invisible.
        const l_bg = ir.bandRadianceC(@floatCast(base_temp));
        for (list.items) |*s| {
            const l = ir.bandRadianceC(@floatCast(s.temp_c));
            s.flux *= if (l > 0.0) util.fmax(1.0 - l_bg / l, 0.0) else 0.0;
        }
        const own_vehicle = blocktemp.seekerVehicle(component);
        // The game's heat records carry no vehicle; drop the ones inside
        // the seeker's own vehicle so it can't lock onto its own engine.
        if (own_vehicle) |vid| {
            var spheres = blocktemp.vehicleSpheres(vid);
            defer spheres.deinit(gpa);
            retain(&list, spheres.items, struct {
                fn keep(sph: []const blocktemp.Sphere, s: *const ir.IrSource) bool {
                    for (sph) |sphere| {
                        const c = sphere.center;
                        if (util.sq(s.x - c[0]) + util.sq(s.y - c[1]) + util.sq(s.z - c[2]) < util.sq(sphere.radius + 0.5)) return false;
                    }
                    return true;
                }
            }.keep);
        }
        sun.addSunSource(origin, &list);
        // Water is opaque to IR: a submerged seeker sees nothing, and
        // nothing submerged is seen.
        const sea = blocktemp.SEA_LEVEL_IR_M;
        if (origin[1] < sea) list.clearRetainingCapacity();
        retain(&list, sea, struct {
            fn keep(level: f64, s: *const ir.IrSource) bool {
                return s.sky or s.y >= level;
            }
        }.keep);
        source_count = list.items.len;
        for (list.items) |*s| ir.projectSource(s, origin, forward, right, up);

        // Line-of-sight: one physics ray per source. Blockers close to the
        // sensor (its own airframe) or to the emitter (its own hull edge)
        // are forgiven so enclosed emitters still glow.
        if (occlude) {
            const handle = lua_api.luaComponentHandle(lua);
            const ctx = if (handle != 0) readUsizeAt(handle + 8) else 0;
            const physics_ok = component != 0 and readUsizeAt(component + 0x1d8) != 0;
            if (physics_ok) {
                const Occlusion = struct { ctx: usize, origin: ir.Vec3, az_center: f64, el_center: f64, fov_rad: f64 };
                retain(&list, Occlusion{ .ctx = ctx, .origin = origin, .az_center = az_center, .el_center = el_center, .fov_rad = fov_rad }, struct {
                    fn keep(o: Occlusion, s: *const ir.IrSource) bool {
                        // Out-of-cone sources are dropped below anyway; a
                        // physics ray each was most of the scan's cost.
                        const d = s.az - o.az_center;
                        const daz = (d - TAU * util.round(d / TAU)) * util.fmax(@cos(s.el), 0.2);
                        if (@sqrt(daz * daz + util.sq(s.el - o.el_center)) > o.fov_rad * 0.5) return true;
                        const center_dist = @sqrt(util.sq(o.origin[0] - s.x) + util.sq(o.origin[1] - s.y) + util.sq(o.origin[2] - s.z));
                        const hit_dist = ir.raycastHit(o.ctx, o.origin, .{ s.x, s.y, s.z }) orelse return true;
                        return !(hit_dist > offsets.OCCLUDE_MIN_DIST and hit_dist < center_dist - s.r_eff - offsets.OCCLUDE_END_SLACK);
                    }
                }.keep);
                source_count = list.items.len;
            }
        }

        // Pre-cone snapshot for verbose diagnostics only.
        if (verbose) {
            diag_fwd = forward;
            diag_fov = fov_rad;
            diag_cone = fov_rad * 0.5;
            diag_steer = .{ az_center, el_center };
            for (list.items) |s| {
                diag_visible.append(gpa, .{ s.az, s.el, s.flux, s.theta, if (s.sky) 1.0 else 0.0 }) catch {};
            }
        }

        // Guidance comes from source directions, not the raster centroid:
        // a sub-pixel emitter can straddle cells symmetrically and make
        // that centroid exactly boresight while the source is off-axis.
        // Only sources inside the configured FOV may steer the seeker. A
        // target outside it leaves the errors at zero, so a missile whose
        // seeker is pointed away flies straight instead of being pulled
        // onto whatever heat source happens to be in front of the hull.
        // The sun steers too: in the cone it outshines any target.
        // Same rule as the original plugin: the single brightest source.
        var brightest_in_cone: ?[3]f64 = null;
        for (list.items) |*s| {
            if (!std.math.isFinite(s.flux) or s.flux <= 0.0) continue;
            const o = coneOffset(s, az_center, el_center);
            if (o.off <= fov_rad * 0.5 and (brightest_in_cone == null or s.flux > brightest_in_cone.?[0])) {
                brightest_in_cone = .{ s.flux, o.daz, o.del };
            }
        }
        if (brightest_in_cone) |best| {
            // Absolute angular offsets in turns, independent of FOV and
            // pixel resolution (0.37 degrees is about 0.00103 turns).
            az_error = best[1] / TAU;
            el_error = best[2] / TAU;
        }
        // Evidence for oscillation: any scan-to-scan error step over
        // 0.0005 turns (0.18 deg), with every in-cone source behind it.
        if (verbose) logErrorStep(seeker_id, az_error, el_error, list.items, origin, az_center, el_center, fov_rad, own_vehicle);

        // The configured FOV still gates the rendered image, detection
        // flux and source count; steering above is independent of raster.
        {
            var keep: usize = 0;
            for (list.items) |s| {
                if (coneOffset(&s, az_center, el_center).off <= fov_rad * 0.5) {
                    total_flux += s.flux;
                    list.items[keep] = s;
                    keep += 1;
                }
            }
            list.shrinkRetainingCapacity(keep);
        }
        source_count = list.items.len;

        // Snapshot the survivors for the heartbeat: distance, effective
        // radius and flux are enough to identify an emitter and back-solve
        // its temperature, which is exactly what a faint reading needs.
        {
            const snapshot = state.LAST_SOURCES.lock();
            defer state.LAST_SOURCES.unlock();
            snapshot.clearRetainingCapacity();
            for (list.items[0..@min(list.items.len, 8)]) |s| {
                const dist = @sqrt(util.sq(s.x - origin[0]) + util.sq(s.y - origin[1]) + util.sq(s.z - origin[2]));
                snapshot.append(gpa, .{ .dist_m = dist, .r_eff = s.r_eff, .flux = s.flux, .sky = s.sky, .temp_c = s.temp_c }) catch {};
            }
        }

        // Rasterize. Unresolved sources (angular radius under one cell)
        // land on the nearest cell with a one-cell spill, so a sub-cell
        // target can never slip between cell centres. Resolved sources
        // dilute per cell (each pixel sees only part of them), which keeps
        // per-pixel brightness radiance-conserving.
        for (list.items) |*s| {
            const o = coneOffset(s, az_center, el_center);
            const col_f = (o.daz / fov_rad + 0.5) * util.f(res_x) - 0.5;
            const row_f = (0.5 - o.del / fov_rad) * util.f(res_y) - 0.5;
            const solid = std.math.pi * s.theta * s.theta;
            const dilut = if (solid <= cell_solid) 1.0 else cell_solid / solid;
            // How far past full scale this source's core would clip, which
            // is the light available to bloom into the surrounding cells.
            const excess = if (gain > 1e-9) util.fmax(s.flux * dilut / gain - 1.0, 0.0) else 0.0;
            const glare = offsets.BLOOM_PEAK * (1.0 - @exp(-excess));
            // Only visit cells the source can reach: its disc (or the
            // 1.2-cell point spill) and, when it blooms, the halo out to
            // 3 radii, where it is below one 12-bit step.
            var rc: f64 = 1.2;
            var rr: f64 = 1.2;
            if (solid > cell_solid) {
                rc = s.theta / (az_sp * util.fmax(@cos(s.el), 0.2));
                rr = s.theta / el_sp;
            }
            if (glare > 0.0) {
                rc = util.fmax(rc, 3.0 * offsets.BLOOM_RADIUS_CELLS);
                rr = util.fmax(rr, 3.0 * offsets.BLOOM_RADIUS_CELLS);
            }
            const c0 = util.f2i(i32, util.fmax(@floor(col_f - rc), 0.0));
            const c1 = @min(util.f2i(i32, @ceil(col_f + rc)), res_x - 1);
            const r0 = util.f2i(i32, util.fmax(@floor(row_f - rr), 0.0));
            const r1 = @min(util.f2i(i32, @ceil(row_f + rr)), res_y - 1);
            var row = r0;
            while (row <= r1) : (row += 1) {
                var col = c0;
                while (col <= c1) : (col += 1) {
                    const dc = col_f - util.f(col);
                    const dr = row_f - util.f(row);
                    const d_cells = @sqrt(dc * dc + dr * dr);
                    const w = if (solid <= cell_solid)
                        util.fclamp(1.2 - d_cells, 0.0, 1.0)
                    else blk: {
                        const a = dc * az_sp * util.fmax(@cos(s.el), 0.2);
                        const b = dr * el_sp;
                        const ang = @sqrt(a * a + b * b);
                        break :blk util.fclamp((s.theta - ang) / (0.3 * s.theta), 0.0, 1.0);
                    };
                    const idx: usize = @intCast(row * res_x + col);
                    const halo = glare * @exp(-util.sq(d_cells / offsets.BLOOM_RADIUS_CELLS)) * gain;
                    const nv = util.fmax(s.flux * dilut * w, halo);
                    if (nv > cells[idx]) cells[idx] = nv;
                }
            }
        }

        // Keep the raw image centroid for diagnostics only. Guidance is
        // computed from source angles above, before FOV rasterization, so a
        // sub-pixel emitter cannot disappear into a symmetric pixel blob.
        for (cells, 0..) |value, index| {
            const i: i32 = @intCast(index);
            const col = util.f(@rem(i, res_x));
            const row = util.f(@divTrunc(i, res_x));
            weight_sum += value;
            weight_x += value * (col + 0.5) / util.f(res_x);
            weight_y += value * (0.5 - (row + 0.5) / util.f(res_y));
        }
        diag_centroid = .{ weight_x / util.fmax(weight_sum, 1e-9), weight_y / util.fmax(weight_sum, 1e-9), weight_sum };

        // Normalize the display image independently of guidance.
        for (cells) |*value| {
            const norm = if (gain > 1e-9) value.* / gain else 0.0;
            value.* = util.fclamp(norm, 0.0, 1.0);
        }
        strength = if (gain > 1e-9) util.fclamp(total_flux / gain, 0.0, 1.0) else 0.0;

        // Publish the rendered frame for the drawing environment, under this
        // seeker's identity. A logic composite is 32 floats - 64 packed
        // cells - so a 64x64 image can only leave the component through the
        // Lua state; ir.frame(id) reads this snapshot from the
        // microcontroller side.
        publishFrame(seeker_id, res_x, res_y, cells);

        // Optional packing for grids above one composite channel set
        // (33..64 cells): 2 cells per float, 12 bits each — value =
        // high*4096 + low. Exact in an f32 composite channel; the receiving
        // micro decodes with floor(v/4096) arithmetic. Resolution and the
        // pack flag are encoded on the composite's bool channels by the
        // component Lua so a monitor script can auto-decode.
        if (pack) {
            var i: usize = 0;
            while (i < cell_count) : (i += 2) {
                const hi = @floor(cells[i] * 4095.0);
                const lo = if (i + 1 < cell_count) @floor(cells[i + 1] * 4095.0) else 0.0;
                packed_vals.append(gpa, hi * 4096.0 + lo) catch {};
            }
        }
    }

    // A scan counter for the throttled dump.
    const scan_index = state.SCAN_COUNT.fetchAdd(1, .monotonic);
    // Verbose diagnostics only exist in the logging build; the player build
    // compiles this whole block out, so a long session leaves a log with a
    // handful of lines instead of megabytes of scan dumps.
    // Per-scan error trace while something is tracked: PN guidance
    // differentiates the line of sight every tick, so tick-to-tick
    // jitter is what matters and the 1-in-300 dump can't show it.
    if (verbose and strength > 0.0) {
        if (TRACE_LINES.fetchAdd(1, .monotonic) < 6000) {
            logf("trace seeker {d}: err=({f},{f}) turns flux={d:.5} n={d}", .{ seeker_id, util.signed(6, az_error), util.signed(6, el_error), total_flux, source_count });
        }
    }
    if (verbose and scan_index % 300 == 0) {
        scanDump(.{
            .source_count = source_count,
            .total_flux = total_flux,
            .base_temp = base_temp,
            .strength = strength,
            .az_error = az_error,
            .el_error = el_error,
            .centroid = diag_centroid,
            .cells = cells,
            .fwd = diag_fwd,
            .fov = diag_fov,
            .cone = diag_cone,
            .steer = diag_steer,
            .visible = diag_visible.items,
            .origin = origin,
        });
    }

    _ = luaPushTable(lua);
    if (cells_out) {
        const values: []const f64 = if (pack and packed_vals.items.len > 0) packed_vals.items else cells;
        for (values, 0..) |value, index| {
            luaPushNumber(lua, value);
            luaRawseti(lua, @as(i64, @intCast(index)) + 1);
        }
    }
    luaPushNumber(lua, az_error);
    luaPushNumber(lua, el_error);
    luaPushNumber(lua, strength);
    luaPushNumber(lua, total_flux);
    luaPushNumber(lua, base_temp);
    luaPushNumber(lua, util.f(source_count));
    luaPushNumber(lua, util.f(seeker_id));
    return 8;
}

/// Keep the sources `keep(ctx, source)` accepts, in order.
fn retain(list: *std.ArrayList(ir.IrSource), ctx: anytype, comptime keep: fn (@TypeOf(ctx), *const ir.IrSource) bool) void {
    var n: usize = 0;
    for (list.items) |s| {
        if (keep(ctx, &s)) {
            list.items[n] = s;
            n += 1;
        }
    }
    list.shrinkRetainingCapacity(n);
}

fn publishFrame(seeker_id: u32, res_x: i32, res_y: i32, cells: []const f64) void {
    var bytes: std.ArrayList(u8) = .empty;
    bytes.ensureTotalCapacity(gpa, cells.len) catch return;
    for (cells) |value| bytes.appendAssumeCapacity(util.f2i(u8, util.fclamp(value, 0.0, 1.0) * 255.0));
    const frames = state.LAST_FRAME.lock();
    defer state.LAST_FRAME.unlock();
    if (frames.count() >= offsets.FRAME_MAP_MAX and !frames.contains(seeker_id)) {
        var it = frames.valueIterator();
        while (it.next()) |frame| frame.bytes.deinit(gpa);
        frames.clearRetainingCapacity();
    }
    const entry = frames.getOrPut(gpa, seeker_id) catch {
        bytes.deinit(gpa);
        return;
    };
    if (entry.found_existing) entry.value_ptr.bytes.deinit(gpa);
    entry.value_ptr.* = .{ .width = res_x, .height = res_y, .bytes = bytes };
}

fn logErrorStep(seeker_id: u32, az_error: f64, el_error: f64, list: []const ir.IrSource, origin: ir.Vec3, az_center: f64, el_center: f64, fov_rad: f64, own_vehicle: ?u32) void {
    const m = LAST_ERR.lock();
    defer LAST_ERR.unlock();
    if (m.get(seeker_id)) |prev| {
        const step = util.fmax(@abs(az_error - prev[0]), @abs(el_error - prev[1]));
        const now = state.nowMs();
        if (step > 0.0005 and now -| LAST_STEP_LOG.load(.monotonic) > 200) {
            LAST_STEP_LOG.store(now, .monotonic);
            var text: std.ArrayList(u8) = .empty;
            defer text.deinit(gpa);
            for (list) |*s| {
                if (coneOffset(s, az_center, el_center).off > fov_rad * 0.5) continue;
                const d = @sqrt(util.sq(s.x - origin[0]) + util.sq(s.y - origin[1]) + util.sq(s.z - origin[2]));
                if (text.items.len > 0) text.append(gpa, ' ') catch {};
                text.print(gpa, "[flux={d:.4} T={d:.0}C d={d:.0}m r={d:.2} az={f} el={f}{s}]", .{
                    s.flux, s.temp_c, d, s.r_eff, util.signed(3, util.degrees(s.az)), util.signed(3, util.degrees(s.el)), if (s.sky) " SKY" else "",
                }) catch {};
            }
            logf("seeker {d}: error step ({f},{f}) -> ({f},{f}) turns steer=({f},{f}) deg own_vehicle={?d} in-cone: {s}", .{
                seeker_id,
                util.signed(5, prev[0]),
                util.signed(5, prev[1]),
                util.signed(5, az_error),
                util.signed(5, el_error),
                util.signed(2, util.degrees(az_center)),
                util.signed(2, util.degrees(el_center)),
                own_vehicle,
                text.items,
            });
        }
    }
    if (m.count() < 256 or m.contains(seeker_id)) m.put(gpa, seeker_id, .{ az_error, el_error }) catch {};
}

const ScanDump = struct {
    source_count: usize,
    total_flux: f64,
    base_temp: f64,
    strength: f64,
    az_error: f64,
    el_error: f64,
    centroid: [3]f64,
    cells: []const f64,
    fwd: ir.Vec3,
    fov: f64,
    cone: f64,
    steer: [2]f64,
    visible: []const [5]f64,
    origin: ir.Vec3,
};

fn scanDump(d: ScanDump) void {
    const ready = state.TEMP_READY.load(.seq_cst);
    var lit: usize = 0;
    var peak: f64 = 0.0;
    for (d.cells) |v| {
        if (v > 0.0) lit += 1;
        peak = util.fmax(peak, v);
    }
    var base_buf: [32]u8 = undefined;
    const base_text = if (ready) std.fmt.bufPrint(&base_buf, "{d:.1} C", .{d.base_temp}) catch "?" else "n/a";
    logf("scan: sources={d} total_flux={d:.5} base={s} strength {d:.3} error=({f},{f}) turns centroid wx/ws={d:.4} wy/ws={d:.4} cells_lit={d} peak={d:.3}", .{
        d.source_count, d.total_flux, base_text, d.strength, util.signed(6, d.az_error), util.signed(6, d.el_error), d.centroid[0], d.centroid[1], lit, peak,
    });
    logf("scan boresight fwd=({d:.3},{d:.3},{d:.3}) fov={d:.1} deg steer=({f},{f}) deg cone=+-{d:.1} deg", .{
        d.fwd[0], d.fwd[1], d.fwd[2], util.degrees(d.fov), util.signed(1, util.degrees(d.steer[0])), util.signed(1, util.degrees(d.steer[1])), util.degrees(d.cone),
    });
    for (d.visible[0..@min(d.visible.len, 4)], 0..) |s, i| {
        const delta = s[0] - d.steer[0];
        const daz = delta - TAU * util.round(delta / TAU);
        const del = s[1] - d.steer[1];
        const off = @sqrt(util.sq(daz * util.fmax(@cos(s[1]), 0.2)) + del * del);
        logf("  dir[{d}]{s}: az={f} deg el={f} deg off={d:.2} deg flux={d:.4}", .{
            i, if (s[4] > 0.5) " SKY" else "", util.signed(2, util.degrees(daz)), util.signed(2, util.degrees(del)), util.degrees(off), s[2],
        });
    }
    if (!ready) {
        if (state.FIRES_HOLDER.load(.monotonic) != 0) {
            logLine("scan: temperature hook not fired yet; fires holder cached (flare/fire only mode)");
        } else {
            logLine("scan: temperature hook has not fired yet (TEMP_READY=false)");
        }
    }
    const sun_state = state.SUN_LOG.load(.monotonic);
    // Detailed sun diagnostics: the game's own rotating sun function output,
    // the light direction the renderer uploaded, and the sky object's sun/moon
    // vectors (scene+0x370+0x238).
    const scene = sun.sceneEnv();
    if (scene >= 0x10000) {
        const st = memory.gameFn(sun.SettingsFn, offsets.RVA_SETTINGS)();
        if (st >= 0x10000) {
            const day_ticks = @as(u64, readU32At(st)) * 3600;
            if (day_ticks > 0) {
                const clock: u64 = readU32At(scene + 0xbaec0);
                const pct = util.f(clock % day_ticks) / util.f(day_ticks);
                var dir = [4]f32{ 0, 0, 0, 0 };
                var m = [_]f32{0} ** 16;
                memory.gameFn(sun.SunRotFn, offsets.RVA_SUNROT)(@floatCast(pct), &dir, &m);
                const nx = -@as(f64, dir[0]);
                const ny = -@as(f64, dir[1]);
                const nz = -@as(f64, dir[2]);
                const nn = util.fmax(@sqrt(nx * nx + ny * ny + nz * nz), 1.0e-9);
                logf("suncalc: scene=0x{x} clock={d} pct={d:.4} hour={d:.2} bf9e0=({f},{f},{f}) to_sun el={f} az={f}", .{
                    scene,                     clock,                     pct,                        pct * 24.0,
                    util.signed(4, dir[0]),    util.signed(4, dir[1]),    util.signed(4, dir[2]),
                    util.signed(1, util.degrees(std.math.asin(ny / nn))),
                    util.signed(1, util.degrees(std.math.atan2(nz / nn, nx / nn))),
                });
            }
        }
        const sx: f64 = readF32At(scene + 0x5a8);
        const sy: f64 = readF32At(scene + 0x5ac);
        const sz: f64 = readF32At(scene + 0x5b0);
        const mx: f64 = readF32At(scene + 0x5b4);
        const my: f64 = readF32At(scene + 0x5b8);
        const mz: f64 = readF32At(scene + 0x5bc);
        const sn = @sqrt(sx * sx + sy * sy + sz * sz);
        const mn = @sqrt(mx * mx + my * my + mz * mz);
        logf("sunsky: sun=({f},{f},{f}) |{d:.4}| el={f} az={f} | moon=({f},{f},{f}) |{d:.4}|", .{
            util.signed(4, sx),                                                                                       util.signed(4, sy), util.signed(4, sz), sn,
            util.signed(1, if (sn > 1.0e-9) util.degrees(std.math.asin(sy / sn)) else 0.0),
            util.signed(1, if (sn > 1.0e-9) util.degrees(std.math.atan2(sz / sn, sx / sn)) else 0.0),
            util.signed(4, mx),                                                                                       util.signed(4, my), util.signed(4, mz), mn,
        });
    }
    if (state.LIGHT_DIR_VALID.load(.monotonic)) {
        const l = blk: {
            const g = state.LIGHT_DIR.lock();
            defer state.LIGHT_DIR.unlock();
            break :blk g.*;
        };
        const ln = util.fmax(@sqrt(l[0] * l[0] + l[1] * l[1] + l[2] * l[2]), 1.0e-9);
        logf("sunlight: dir=({f},{f},{f}) el={f} az={f}", .{
            util.signed(4, l[0]), util.signed(4, l[1]), util.signed(4, l[2]),
            util.signed(1, util.degrees(std.math.asin(l[1] / ln))),
            util.signed(1, util.degrees(std.math.atan2(l[2] / ln, l[0] / ln))),
        });
    }
    if (sun_state == 0) {
        logLine("sun: no scene cached yet (waiting for the render uniforms hook)");
    } else if (sun_state == 1) {
        logLine("sun: below the horizon");
    } else {
        const p = util.f(sun_state - 2) / 1000.0;
        logf("sun: day_percent={d:.3} hour={d:.1} emitter active el={f} deg az={f} deg", .{
            p, p * 24.0, util.signed(1, util.f(state.SUN_EL.load(.monotonic)) / 10.0), util.signed(1, util.f(state.SUN_AZ.load(.monotonic)) / 10.0),
        });
    }
    {
        const live_scene = state.SCENE.load(.monotonic);
        const weather = ir.weatherFogRain();
        const fog = weather[0];
        const rain = weather[1];
        if (live_scene >= 0x10000) {
            const w = live_scene + 0xbade8;
            logf("weather: fog={d:.3} rain={d:.3} (live {d:.3}/{d:.3}) wind={d:.3} dir={f} sigma={d:.5}/m", .{
                fog,                                                             rain,
                readF32At(w + 0x90),                                             readF32At(w + 0x98),
                util.fmax(readF32At(w + 0xa0), readF32At(w + 0xa4)),             util.signed(3, readF32At(w + 0xa8)),
                @as(f64, fog) * offsets.FOG_SIGMA_PER_M + @as(f64, rain) * offsets.RAIN_SIGMA_PER_M,
            });
            logf("weather: sun transmittance={d:.5} (fog sigma {d:.5}/m over {d:.0} m)", .{ ir.weatherTransmittance(fog, rain, offsets.SUN_DIST, true), offsets.FOG_SIGMA_SKY_PER_M, offsets.SKY_PATH_CLAMP_M });
        } else {
            logLine("weather: scene not cached yet");
        }
    }
    logf("scan origin=({d:.1},{d:.1},{d:.1})", .{ d.origin[0], d.origin[1], d.origin[2] });
    const vcount = @min(state.VARIANT_COUNT.load(.monotonic), state.MAX_VARIANTS);
    logf("scan: {d} heat container variant(s)", .{vcount});
    for (0..vcount) |i| {
        const a = heat.variantArgs(i);
        logf("  variant[{d}] heat=0x{x} filter=0x{x}", .{ i, a[0], a[7] });
        heat.logHeatContainer(a[0]);
        heat.logVolumeHeatSources(a[0]);
    }
    // Dump the fires holder the scan actually uses: the cached variant
    // when the temperature hook has run, the fire-render holder otherwise.
    heat.logFiresContainer(if (vcount > 0) heat.variantArgs(0)[3] else state.FIRES_HOLDER.load(.monotonic));
}
