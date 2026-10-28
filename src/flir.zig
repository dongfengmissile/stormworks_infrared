//! FLIR camera view (see `docs/FLIR_DESIGN.md`).
//!
//! The game's own "Infrared Mode" is only a tonemap grading (`color_mode = 1`
//! plus grain/vignette); no thermal pass exists behind it. This module adds one,
//! and only for views whose grading group is the IR signature (see
//! `tonemapParamsIr`) or, in a debug build, for the main view while F10 is
//! toggled on:
//!
//!   * `irTonemapHook` (0x68b8c0) uploads the game's tonemap uniforms and
//!     decides whether THIS view is IR-graded from its own `params` argument (the
//!     same color_mode/grain/vignette signature `view_state+0x870` carries), then
//!     tags the tonemap quad that follows it in GL order. The scene-submit hook
//!     is only a frame counter; no view fact is keyed by submit.
//!   * The `glBindFramebuffer` thunk tracks the bound draw target and the last
//!     G-buffer-like target bound in GL order; at the tonemap quad draw
//!     (`glDrawElements` thunk, return address) that target is paired with its
//!     G-buffer and inherits the IR state of the quad's own params, and the quad
//!     is replaced with the FLIR composite when its target is IR-graded (or F10
//!     forces a target learned as the main view: the view whose first framebuffer
//!     bind after its quad is FBO 0, where FXAA presents).
//!   * In the same thunk, a draw of a vehicle chunk mesh (index buffer mapped to
//!     a body on the game thread) inside a G-buffer whose paired target is
//!     IR-forced is re-issued with our copy of the opaque program, which samples
//!     the body's R32F 3D temperature texture and writes the opaque pass's
//!     G-buffer packing: the world-space normal xyz plus the vehicle marker in the
//!     light-factor channel (attachment 0 is `gnormal_light_factor` = normal.xyz +
//!     light factor in .w, per `opaque.glslf`), the encoded temperature to 1 and
//!     zero velocity to 2.
//!   * The scene-submit detour (0x6BB400, entry stub saving RBX/R15/RSI) maps each
//!     view's persistent render-target holder to its camera; the tonemap uniforms
//!     hook then enables FLIR iff `state.flirEnabledCfg(camera)` is non-null.
//!
//! Everything unverified at RE time (G-buffer attachment identities, the
//! `body+0x540` container layout, the vertex frame) is checked at runtime and
//! reported once in a debug build; any failure falls back to the game's normal
//! rendering for that view rather than drawing garbage.
//!
//! GL is reached through `opengl32.dll`/`wglGetProcAddress` on the game's own
//! GL thread (our hooks run there), so no GLEW global is needed to call GL.
//! Only the dispatch *slots* the game itself calls through (`glDrawElements`'s
//! import slot, the GLEW `glUniformMatrix4fv` and `glBindFramebuffer` globals)
//! are re-pointed at our thunks.

const std = @import("std");
const blocktemp = @import("blocktemp.zig");
const fault = @import("fault.zig");
const fx = @import("fx.zig");
const heat = @import("heat.zig");
const hooks = @import("hooks.zig");
const logging = @import("logging.zig");
const memory = @import("memory.zig");
const minhook = @import("minhook.zig");
const offsets = @import("offsets.zig");
const state = @import("state.zig");
const sun = @import("sun.zig");
const util = @import("util.zig");
const win = @import("win32.zig");

const Atomic = std.atomic.Value;
const gpa = state.gpa;
const verbose = logging.verbose;
const logLine = logging.logLine;
const logf = logging.logf;
const logOnce = logging.logOnce;
const stage = fault.stage;
const readable = memory.readable;
const readUsizeAt = memory.readUsizeAt;
const readU32At = memory.readU32At;
const readF64At = memory.readF64At;
const PTR_MIN = memory.PTR_MIN;
const Mutex = win.Mutex;

// ---------------------------------------------------------------------------
// GL constants
// ---------------------------------------------------------------------------

const GL_TRIANGLES: u32 = 0x0004;
const GL_FLOAT: u32 = 0x1406;
const GL_UNSIGNED_SHORT: u32 = 0x1403;
const GL_RED: u32 = 0x1903;
const GL_R32F: u32 = 0x822e;
const GL_TEXTURE_2D: u32 = 0x0de1;
const GL_TEXTURE_3D: u32 = 0x806f;
const GL_TEXTURE0: u32 = 0x84c0;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_TEXTURE_WRAP_S: u32 = 0x2802;
const GL_TEXTURE_WRAP_T: u32 = 0x2803;
const GL_TEXTURE_WRAP_R: u32 = 0x8072;
const GL_NEAREST: i32 = 0x2600;
const GL_CLAMP_TO_EDGE: i32 = 0x812f;
const GL_VERTEX_SHADER: u32 = 0x8b31;
const GL_FRAGMENT_SHADER: u32 = 0x8b30;
const GL_COMPILE_STATUS: u32 = 0x8b81;
const GL_LINK_STATUS: u32 = 0x8b82;
const GL_INFO_LOG_LENGTH: u32 = 0x8b84;
const GL_FRAMEBUFFER: u32 = 0x8d40;
const GL_DRAW_FRAMEBUFFER: u32 = 0x8ca9;
const GL_FRAMEBUFFER_BINDING: u32 = 0x8ca6;
const GL_COLOR_ATTACHMENT0: u32 = 0x8ce0;
const GL_DEPTH_ATTACHMENT: u32 = 0x8d00;
const GL_DEPTH_STENCIL_ATTACHMENT: u32 = 0x821a;
const GL_DEPTH_TEST: u32 = 0x0b71;
const GL_BLEND: u32 = 0x0be2;
const GL_CULL_FACE: u32 = 0x0b44;
const GL_VIEWPORT: u32 = 0x0ba2;
const GL_DEPTH_BUFFER_BIT: u32 = 0x0000_0100;
const GL_ENABLE_BIT: u32 = 0x0000_2000;
const GL_COLOR_BUFFER_BIT: u32 = 0x0000_4000;
const GL_TEXTURE_BIT: u32 = 0x0004_0000;
const GL_CLIENT_PIXEL_STORE_BIT: u32 = 0x0000_0001;
const GL_CURRENT_PROGRAM: u32 = 0x8b8d;
const GL_VERTEX_ARRAY_BINDING: u32 = 0x85b5;
const GL_ACTIVE_TEXTURE: u32 = 0x84e0;
const GL_TEXTURE_BINDING_3D: u32 = 0x806a;
const GL_ELEMENT_ARRAY_BUFFER_BINDING: u32 = 0x8895;
const GL_FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE: u32 = 0x8cd0;
const GL_FRAMEBUFFER_ATTACHMENT_OBJECT_NAME: u32 = 0x8cd1;
const GL_FRAMEBUFFER_ATTACHMENT_COMPONENT_TYPE: u32 = 0x8211;
const GL_TEXTURE: u32 = 0x1702;
const GL_NONE: i32 = 0;
const GL_UNPACK_ALIGNMENT: u32 = 0x0cf5;
const GL_UNPACK_ROW_LENGTH: u32 = 0x0cf2;
const GL_UNPACK_IMAGE_HEIGHT: u32 = 0x806e;
const GL_UNPACK_SKIP_ROWS: u32 = 0x0cf3;
const GL_UNPACK_SKIP_PIXELS: u32 = 0x0cf4;
const GL_UNPACK_SKIP_IMAGES: u32 = 0x806d;
const GL_PIXEL_UNPACK_BUFFER: u32 = 0x88ec;
const GL_PIXEL_UNPACK_BUFFER_BINDING: u32 = 0x88ef;
const GL_TEXTURE_BINDING_2D: u32 = 0x8069;
const GL_TEXTURE_INTERNAL_FORMAT: u32 = 0x1003;
const GL_TEXTURE_WIDTH: u32 = 0x1000;
const GL_TEXTURE_HEIGHT: u32 = 0x1001;
const GL_DEPTH_WRITEMASK: u32 = 0x0b72;
const GL_COLOR_WRITEMASK: u32 = 0x0c23;
const GL_RGBA: u32 = 0x1908;
const GL_READ_FRAMEBUFFER: u32 = 0x8ca8;
const GL_READ_FRAMEBUFFER_BINDING: u32 = 0x8caa;

// ---------------------------------------------------------------------------
// GL entry points (resolved lazily on the GL thread, `opengl32` + wgl)
// ---------------------------------------------------------------------------

const API: std.builtin.CallingConvention = .winapi;

/// Every GL entry point the plugin calls. A field `gen_vertex_arrays` is loaded
/// as `glGenVertexArrays` (see `glEntryName`).
const Gl = struct {
    create_shader: *const fn (u32) callconv(API) u32,
    shader_source: *const fn (u32, i32, [*]const [*]const u8, ?[*]const i32) callconv(API) void,
    compile_shader: *const fn (u32) callconv(API) void,
    get_shaderiv: *const fn (u32, u32, *i32) callconv(API) void,
    get_shader_info_log: *const fn (u32, i32, ?*i32, [*]u8) callconv(API) void,
    create_program: *const fn () callconv(API) u32,
    attach_shader: *const fn (u32, u32) callconv(API) void,
    link_program: *const fn (u32) callconv(API) void,
    get_programiv: *const fn (u32, u32, *i32) callconv(API) void,
    get_program_info_log: *const fn (u32, i32, ?*i32, [*]u8) callconv(API) void,
    delete_shader: *const fn (u32) callconv(API) void,
    delete_program: *const fn (u32) callconv(API) void,
    bind_attrib_location: *const fn (u32, u32, [*:0]const u8) callconv(API) void,
    bind_frag_data_location: *const fn (u32, u32, [*:0]const u8) callconv(API) void,
    get_uniform_location: *const fn (u32, [*:0]const u8) callconv(API) i32,
    get_uniformfv: *const fn (u32, i32, [*]f32) callconv(API) void,
    get_uniformiv: *const fn (u32, i32, [*]i32) callconv(API) void,
    get_attached_shaders: *const fn (u32, i32, ?*i32, [*]u32) callconv(API) void,
    get_shader_source: *const fn (u32, i32, ?*i32, [*]u8) callconv(API) void,
    get_attrib_location: *const fn (u32, [*:0]const u8) callconv(API) i32,
    use_program: *const fn (u32) callconv(API) void,
    uniform1i: *const fn (i32, i32) callconv(API) void,
    uniform1f: *const fn (i32, f32) callconv(API) void,
    uniform2f: *const fn (i32, f32, f32) callconv(API) void,
    uniform3f: *const fn (i32, f32, f32, f32) callconv(API) void,
    uniform4fv: *const fn (i32, i32, [*]const f32) callconv(API) void,
    uniform3i: *const fn (i32, i32, i32, i32) callconv(API) void,
    uniform_matrix4fv: *const fn (i32, i32, u8, [*]const f32) callconv(API) void,
    gen_textures: *const fn (i32, [*]u32) callconv(API) void,
    delete_textures: *const fn (i32, [*]const u32) callconv(API) void,
    bind_texture: *const fn (u32, u32) callconv(API) void,
    active_texture: *const fn (u32) callconv(API) void,
    tex_parameteri: *const fn (u32, u32, i32) callconv(API) void,
    tex_image_3d: *const fn (u32, i32, i32, i32, i32, i32, i32, u32, u32, ?*const anyopaque) callconv(API) void,
    tex_sub_image_3d: *const fn (u32, i32, i32, i32, i32, i32, i32, i32, u32, u32, ?*const anyopaque) callconv(API) void,
    pixel_storei: *const fn (u32, i32) callconv(API) void,
    bind_buffer: *const fn (u32, u32) callconv(API) void,
    gen_vertex_arrays: *const fn (i32, [*]u32) callconv(API) void,
    bind_vertex_array: *const fn (u32) callconv(API) void,
    draw_arrays: *const fn (u32, i32, i32) callconv(API) void,
    draw_elements: *const fn (u32, i32, u32, ?*const anyopaque) callconv(API) void,
    get_integerv: *const fn (u32, [*]i32) callconv(API) void,
    enable: *const fn (u32) callconv(API) void,
    disable: *const fn (u32) callconv(API) void,
    is_enabled: *const fn (u32) callconv(API) u8,
    get_booleanv: *const fn (u32, [*]u8) callconv(API) void,
    depth_mask: *const fn (u8) callconv(API) void,
    color_mask: *const fn (u8, u8, u8, u8) callconv(API) void,
    read_pixels: *const fn (i32, i32, i32, i32, u32, u32, ?*anyopaque) callconv(API) void,
    bind_framebuffer: *const fn (u32, u32) callconv(API) void,
    get_framebuffer_attachment_parameteriv: *const fn (u32, u32, u32, *i32) callconv(API) void,
    get_tex_level_parameteriv: *const fn (u32, i32, u32, *i32) callconv(API) void,
    get_vertex_attribiv: *const fn (u32, u32, *i32) callconv(API) void,
    get_vertex_attrib_pointerv: *const fn (u32, u32, *?*const anyopaque) callconv(API) void,
    get_buffer_sub_data: *const fn (u32, isize, isize, ?*anyopaque) callconv(API) void,
    get_buffer_parameteriv: *const fn (u32, u32, *i32) callconv(API) void,
    gen_buffers: *const fn (i32, [*]u32) callconv(API) void,
    buffer_data: *const fn (u32, isize, ?*const anyopaque, u32) callconv(API) void,
    vertex_attrib_pointer: *const fn (u32, i32, u32, u8, i32, ?*const anyopaque) callconv(API) void,
    enable_vertex_attrib_array: *const fn (u32) callconv(API) void,
    tex_image_2d: *const fn (u32, i32, i32, i32, i32, i32, u32, u32, ?*const anyopaque) callconv(API) void,
    // Compat-context attribute stacks: save/restore state without a query
    // (each glGet* forces a driver sync under Proton).
    push_attrib: *const fn (u32) callconv(API) void,
    pop_attrib: *const fn () callconv(API) void,
    push_client_attrib: *const fn (u32) callconv(API) void,
    pop_client_attrib: *const fn () callconv(API) void,

    fn load() ?Gl {
        var table: Gl = undefined;
        inline for (@typeInfo(Gl).@"struct".fields) |field| {
            const name = comptime glEntryName(field.name);
            const raw = glSymbol(name);
            if (raw == 0) {
                logDebug("FLIR: GL entry point missing: " ++ field.name ++ "; FLIR disabled", .{});
                return null;
            }
            @field(table, field.name) = @ptrFromInt(raw);
        }
        return table;
    }
};

var GL_TABLE: Gl = undefined;
/// 0 = not loaded yet, 1 = loaded, 2 = a required entry point is missing.
var GL_STATE: Atomic(u8) = .init(0);

/// Driver queries issued by the plugin (each one can stall a threaded GL
/// driver); reported in the 5 s counters line.
var GL_QUERIES: Atomic(u64) = .init(0);
/// Time spent inside the plugin's draw-thunk body (verbose builds), microseconds.
var HOOK_US: Atomic(u64) = .init(0);
/// Part of HOOK_US spent in draws we substituted (vs. checking and skipping).
var HOOK_SUB_US: Atomic(u64) = .init(0);

/// Driver queries per source line since the last perf line (verbose builds),
/// so a remaining sync can be traced to its call site.
const QuerySite = struct { line: u32, count: u32 };
var GLQ_SITES: Mutex(std.ArrayList(QuerySite)) = .{ .value = .empty };

fn noteQuerySite(line: u32) void {
    const sites = GLQ_SITES.lock();
    defer GLQ_SITES.unlock();
    for (sites.items) |*e| {
        if (e.line == line) {
            e.count += 1;
            return;
        }
    }
    if (sites.items.len < 128) sites.append(gpa, .{ .line = line, .count = 1 }) catch {};
}

/// Count one GL query (call right before the query entry point).
inline fn glq(comptime src: std.builtin.SourceLocation) void {
    _ = GL_QUERIES.fetchAdd(1, .monotonic);
    if (verbose) noteQuerySite(src.line);
}

// GL state tracked from the game's own calls through the GLEW slots, so the
// per-draw path never has to ask the driver. -1 = unknown (query once, then
// tracked). Only the render thread issues GL.
var TRACK_PROGRAM: Atomic(i64) = .init(-1);
var TRACK_IBO: Atomic(i64) = .init(-1);
var TRACK_ACTIVE_TEX: Atomic(i64) = .init(-1);
var ORIG_GL_USE_PROGRAM: Atomic(usize) = .init(0);
var ORIG_GL_BIND_BUFFER: Atomic(usize) = .init(0);
var ORIG_GL_ACTIVE_TEXTURE: Atomic(usize) = .init(0);
var ORIG_GL_BIND_VERTEX_ARRAY: Atomic(usize) = .init(0);
var ORIG_GL_VIEWPORT: Atomic(usize) = .init(0);
/// Game's GL_PIXEL_UNPACK_BUFFER binding (-1 = unknown) and viewport.
var TRACK_UNPACK_PBO: Atomic(i64) = .init(-1);
var TRACK_VIEWPORT: Mutex(?[4]i32) = .{ .value = null };

/// Tracked value, or one counted query (then tracked) while unknown.
fn tracked(g: *const Gl, slot: *Atomic(i64), pname: u32) i32 {
    const v = slot.load(.monotonic);
    if (v >= 0) return @truncate(v);
    var out: i32 = 0;
    glq(@src());
    g.get_integerv(pname, @ptrCast(&out));
    // Only cache once the game's own calls are being tracked (slot hooked).
    const hooked = switch (pname) {
        GL_CURRENT_PROGRAM => ORIG_GL_USE_PROGRAM.load(.monotonic) != 0,
        GL_ELEMENT_ARRAY_BUFFER_BINDING, GL_PIXEL_UNPACK_BUFFER_BINDING => ORIG_GL_BIND_BUFFER.load(.monotonic) != 0,
        GL_ACTIVE_TEXTURE => ORIG_GL_ACTIVE_TEXTURE.load(.monotonic) != 0,
        else => false,
    };
    if (hooked) slot.store(out, .monotonic);
    return out;
}

fn glUseProgramHook(program: u32) callconv(API) void {
    const original = ORIG_GL_USE_PROGRAM.load(.monotonic);
    if (original != 0) {
        const f: *const fn (u32) callconv(API) void = @ptrFromInt(original);
        f(program);
    }
    TRACK_PROGRAM.store(program, .monotonic);
}

fn glBindBufferHook(target: u32, buffer: u32) callconv(API) void {
    const original = ORIG_GL_BIND_BUFFER.load(.monotonic);
    if (original != 0) {
        const f: *const fn (u32, u32) callconv(API) void = @ptrFromInt(original);
        f(target, buffer);
    }
    if (target == 0x8893) {
        TRACK_IBO.store(buffer, .monotonic);
    } else if (target == GL_PIXEL_UNPACK_BUFFER) {
        TRACK_UNPACK_PBO.store(buffer, .monotonic);
    }
}

fn glViewportHook(x: i32, y: i32, w: i32, h: i32) callconv(API) void {
    const original = ORIG_GL_VIEWPORT.load(.monotonic);
    if (original != 0) {
        const f: *const fn (i32, i32, i32, i32) callconv(API) void = @ptrFromInt(original);
        f(x, y, w, h);
    }
    const v = TRACK_VIEWPORT.lock();
    defer TRACK_VIEWPORT.unlock();
    v.* = .{ x, y, w, h };
}

/// Current viewport: tracked from the game's `glViewport` (IAT), else queried.
fn currentViewport(g: *const Gl) [4]i32 {
    {
        const v = TRACK_VIEWPORT.lock();
        defer TRACK_VIEWPORT.unlock();
        if (v.*) |viewport| return viewport;
    }
    var v = [4]i32{ 0, 0, 0, 0 };
    glq(@src());
    g.get_integerv(GL_VIEWPORT, &v);
    return v;
}

var ORIG_GL_BIND_TEXTURE: Atomic(usize) = .init(0);
/// Last GL_TEXTURE_2D bound per texture unit (game's glBindTexture, IAT).
var TEX_UNITS: [16]Atomic(u32) = [_]Atomic(u32){.init(0)} ** 16;

fn glBindTextureHook(target: u32, texture: u32) callconv(API) void {
    const original = ORIG_GL_BIND_TEXTURE.load(.monotonic);
    if (original != 0) {
        const f: *const fn (u32, u32) callconv(API) void = @ptrFromInt(original);
        f(target, texture);
    }
    if (target == GL_TEXTURE_2D) {
        const unit = TRACK_ACTIVE_TEX.load(.monotonic) - GL_TEXTURE0;
        if (unit >= 0 and unit < 16) TEX_UNITS[@intCast(unit)].store(texture, .monotonic);
    }
}

fn glActiveTextureHook(texture: u32) callconv(API) void {
    const original = ORIG_GL_ACTIVE_TEXTURE.load(.monotonic);
    if (original != 0) {
        const f: *const fn (u32) callconv(API) void = @ptrFromInt(original);
        f(texture);
    }
    TRACK_ACTIVE_TEX.store(texture, .monotonic);
}

fn glBindVertexArrayHook(vao: u32) callconv(API) void {
    const original = ORIG_GL_BIND_VERTEX_ARRAY.load(.monotonic);
    if (original != 0) {
        const f: *const fn (u32) callconv(API) void = @ptrFromInt(original);
        f(vao);
    }
    // The element binding is VAO state: unknown until the next bind/query.
    TRACK_IBO.store(-1, .monotonic);
}

var GL_BASE: Atomic(usize) = .init(0);
/// Per-hook switches (`infrared_disable_flir_draw` / `_matrix` / `_fbo` next to
/// the DLL): each GL dispatch slot can be left untouched on its own, so a
/// rendering problem can be bisected down to a single thunk without a rebuild.
/// Read at install time; `ensureGlSlots` runs again from the render thread, so
/// the decision has to live in statics.
var DRAW_THUNK_DISABLED: Atomic(bool) = .init(false);
var MATRIX_THUNK_DISABLED: Atomic(bool) = .init(false);
var FBO_THUNK_DISABLED: Atomic(bool) = .init(false);

/// GL table, or null when a required entry point cannot be resolved. The
/// first call also retries the GLEW dispatch-slot patches: if the plugin was
/// injected before GLEW initialised, the slots were still null at install
/// time and are only usable from the render thread anyway. Render thread only.
fn gl() ?*const Gl {
    switch (GL_STATE.load(.acquire)) {
        1 => return &GL_TABLE,
        2 => return null,
        else => {},
    }
    ensureGlSlots();
    if (Gl.load()) |table| {
        GL_TABLE = table;
        GL_STATE.store(1, .release);
        return &GL_TABLE;
    }
    GL_STATE.store(2, .release);
    return null;
}

/// `gen_vertex_arrays` -> `glGenVertexArrays`: the GL entry points are
/// exported (or returned by wglGetProcAddress) under exactly those names.
fn glEntryName(comptime field: []const u8) [:0]const u8 {
    comptime {
        var name: []const u8 = "gl";
        var it = std.mem.splitScalar(u8, field, '_');
        while (it.next()) |part| {
            if (part.len == 0) continue;
            var piece: [part.len]u8 = part[0..part.len].*;
            piece[0] = std.ascii.toUpper(piece[0]);
            // `2d` / `3d` are spelled `2D` / `3D` in the GL names.
            if (std.ascii.isDigit(part[0])) {
                for (piece[1..]) |*c| c.* = std.ascii.toUpper(c.*);
            }
            name = name ++ piece;
        }
        const final = name[0..name.len].* ++ [_]u8{0};
        return final[0..name.len :0];
    }
}

var OGL_MODULE: Atomic(usize) = .init(0);
var WGL_GET_PROC: Atomic(usize) = .init(0);

fn glSymbol(name: [:0]const u8) usize {
    var module = OGL_MODULE.load(.monotonic);
    if (module == 0) {
        module = @intFromPtr(win.LoadLibraryA("opengl32.dll"));
        OGL_MODULE.store(module, .monotonic);
    }
    if (module == 0) return 0;
    if (win.GetProcAddress(@ptrFromInt(module), name)) |direct| return @intFromPtr(direct);
    var get_proc = WGL_GET_PROC.load(.monotonic);
    if (get_proc == 0) {
        get_proc = if (win.GetProcAddress(@ptrFromInt(module), "wglGetProcAddress")) |f| @intFromPtr(f) else 0;
        WGL_GET_PROC.store(get_proc, .monotonic);
    }
    if (get_proc == 0) return 0;
    const f: *const fn ([*:0]const u8) callconv(API) usize = @ptrFromInt(get_proc);
    // wglGetProcAddress returns small sentinel values (1, 2, 3, -1) for
    // functions it does not know; never accept those as addresses.
    const p = f(name);
    return if (p > 0x10000 and p != std.math.maxInt(usize)) p else 0;
}

// ---------------------------------------------------------------------------
// Shaders (copies of the game's opaque program, plus the composite)
// ---------------------------------------------------------------------------

/// Copy of `graphics/shaders/opaque.glslv` (position/normal attributes, log
/// depth). Chunk vertices are body-local absolute voxel-centre*0.25 (faces at +-0.125) (verified: the
/// chunk origin is a constant zero in `0x4A4430`), so `round(pos/0.25)` is the
/// voxel coordinate of the temperature grid. The 3D texture is sampled in the
/// fragment shader with `texelFetch` (implicit LOD is undefined in a vertex
/// shader).
const BLOCK_VS: [:0]const u8 = @embedFile("shaders/block.vert");
const BLOCK_FS: [:0]const u8 = @embedFile("shaders/block.frag");
/// The game's own tonemap quad geometry (tonemap.glslv): the texcoords the game
/// feeds define the exact G-buffer region this view shows.
const COMPOSITE_VS: [:0]const u8 = @embedFile("shaders/composite.vert");
const COMPOSITE_FS: [:0]const u8 = @embedFile("shaders/composite.frag");

// ---------------------------------------------------------------------------
// Runtime state
// ---------------------------------------------------------------------------

const BlockProgram = struct {
    program: u32,
    mat_view_proj: i32,
    mat_world: i32,
    comp_to_body: i32,
    grid_min: i32,
    grid_dims: i32,
    enc_min: i32,
    enc_max: i32,
    tex_temps: i32,
    normal_encoded: i32,
    comp_voxel: i32,
    has_comp_voxel: i32,
    has_color: i32,
    color_loc: i32,
};

/// The composite program and its uniform locations. It draws into whichever
/// framebuffer the game has bound at the tonemap quad (the tonemap output RT),
/// so there is no render target of our own.
const Composite = struct {
    program: u32,
    tex_normal: i32,
    tex_color: i32,
    tex_depth: i32,
    has_depth: i32,
    mat_view_proj: i32,
    normal_encoded: i32,
    sun_dir: i32,
    sun_valid: i32,
    day_pct: i32,
    clear_sky: i32,
    mat_inv: i32,
    vehicle_air_c: i32,
    tex_water: i32,
    has_water: i32,
    plume_a: i32,
    plume_b: i32,
    plume_n: i32,
    has_inv: i32,
    water_y: i32,
    ambient_c: i32,
    sun_soak_c: i32,
    tint: i32,
    black_hot: i32,
    tmin: i32,
    tmax: i32,
    enc_min: i32,
    enc_max: i32,
    sky_c: i32,
    sky_depth_m: i32,
    /// Attribute locations the program was linked with (the game tonemap's).
    pos_loc: u32,
    coord_loc: u32,
};

/// The opaque G-buffer target: attachment 0 is `gnormal_light_factor`, 1 is
/// `gcolor` (the fixed `glDrawBuffers` list and the opaque shader's output order
/// are verified in RE_NOTES §1.3.4/§1.2.2). `normal_encoded` records whether
/// attachment 0 is a normalized format: the packed normal.xy is then decoded
/// with `*2-1`, and the vehicle marker has to live in `gcolor` alpha instead of
/// the light-factor channel (a normalized attachment cannot hold -1).
const Gbuffer = struct {
    fbo: u32 = 0,
    tex_normal: u32 = 0,
    tex_color: u32 = 0,
    depth: u32 = 0,
    /// Texel size of the colour-0 attachment: diagnostics only. The composite
    /// samples the G-buffer with normalised UVs, so it does not require the
    /// tonemap viewport to have the same size, and the view a G-buffer belongs to
    /// is decided by the pairing learned at that view's tonemap quad (`Targets`),
    /// never by a size comparison.
    size: [2]i32 = .{ 0, 0 },
    normal_encoded: bool = false,
};

const SavedState = struct {
    program: i32,
    active_texture: i32,
};

/// Our 3D texture currently bound on FLIR_TEXTURE_UNIT (0: unknown/none). No
/// game shader samples a 3D texture, so it stays bound between draws instead
/// of a bind + unbind per substituted draw.
var LAST_3D: Atomic(u32) = .init(0);

/// A matrix upload: 16 raw floats and the transpose flag the game used.
const Mat = struct { m: [16]f32, transpose: u8 };

/// Uniform values last uploaded to each block program. Uniforms persist per
/// program and only our substitute uses these programs, so unchanged values
/// (same view, same body for consecutive chunks) skip the GL call.
const BlockUni = struct {
    vp: Mat,
    world: Mat,
    comp: [16]f32,
    min: [3]i32,
    dims: [3]u32,
    normal_encoded: bool,
    comp_voxel: [3]i32,
    is_comp: bool,
};
const BlockUniEntry = struct { program: u32, uni: BlockUni };
var BLOCK_UNI: Mutex(std.ArrayList(BlockUniEntry)) = .{ .value = .empty };

/// Set while we re-issue the draw with our own program, so nothing (including a
/// nested GL hook) can substitute into our own draw.
var IN_SUBSTITUTE: Atomic(bool) = .init(false);
/// A failure in the block pass disables it for the session: the GL state of a
/// half-finished substitute draw cannot be trusted.
var BLOCK_FAILED: Atomic(bool) = .init(false);

/// Keyed by the game's opaque-program id: attribute locations are per program,
/// so a hit needs no GL queries. A failed build is cached as null so a broken
/// shader is not rebuilt (and its info log not rewritten) on every draw.
const BlockProgramEntry = struct { game_program: u32, program: ?BlockProgram };
var BLOCK_PROGRAMS: Mutex(std.ArrayList(BlockProgramEntry)) = .{ .value = .empty };
/// Programs seen drawing a mapped vehicle chunk mesh in the G-buffer pass. The
/// opaque variants share the attribute/uniform layout, so a program that has
/// `vertex_position_in`, `vertex_normal_in` and `mat_world` qualifies.
/// program -> has the opaque layout. Negatives are cached too: an uncached
/// rejection cost three driver queries on every draw of that program.
var OPAQUE_PROGRAMS: Mutex(fx.HashMap(u32, bool)) = .{ .value = .empty };
var COMPOSITE: Mutex(?Composite) = .{ .value = null };
/// Keyed by the chunk mesh's GL index buffer id (`mesh+0x64`). Built on the game
/// thread; the render thread only looks up the published map.
var MESH_BODY: Mutex(fx.HashMap(u32, usize)) = .{ .value = .empty };
/// Lock-free mirror of "the published mesh->body map is empty", so the per-draw
/// gate can skip the whole block pass without taking the map lock or issuing a
/// single GL query (an empty map means no draw can ever be substituted).
var MESH_BODY_EMPTY: Atomic(bool) = .init(true);

/// One R32F volume per body. `frame` is the submit counter of the last upload,
/// which throttles uploads to one per body per `FLIR_UPLOAD_INTERVAL_FRAMES`.
/// `min`/`dims` describe the texture that is actually resident.
const GridTexture = struct {
    texture: u32,
    version: u64,
    min: [3]i32,
    dims: [3]u32,
    frame: u64,
};

var GRID_TEXTURES: Mutex(fx.HashMap(usize, GridTexture)) = .{ .value = .empty };
/// Staging copy used for the volume upload. The render thread is its only user,
/// so holding it across the `glTexImage3D` cannot block the sim thread, and the
/// simulated grid itself is never cloned.
var UPLOAD_BUF: Mutex(std.ArrayList(f32)) = .{ .value = .empty };

/// Last `mat_view_proj` / `mat_world` uploaded to each program (raw floats and
/// transpose flag), plus the program's two uniform locations (looked up once).
/// A uniform is program state, so this is exactly what a draw with that program
/// uses - without a per-draw `glGetUniformfv`. One lock per matrix upload: the
/// hook runs for every upload in the frame while FLIR is on.
const ProgMats = struct {
    /// (mat_view_proj, mat_world) locations.
    locs: [2]i32 = .{ -1, -1 },
    /// mat_view_proj_inverse location of the deferred light program (else -1).
    light_inv: i32 = -1,
    vp: ?Mat = null,
    world: ?Mat = null,
};
var PROG_MATS: Mutex(fx.HashMap(u32, ProgMats)) = .{ .value = .empty };
/// Raw 16 floats of the last `mat_view_proj` / `mat_world` upload, replayed with
/// the same transpose flag the game used (identical transform).
var VIEW_PROJ: Mutex([16]f32) = .{ .value = [_]f32{0} ** 16 };
var VIEW_PROJ_TRANSPOSE: Atomic(u32) = .init(0);
var VIEW_PROJ_VALID: Atomic(bool) = .init(false);
var WORLD_MATRIX: Mutex([16]f32) = .{ .value = [_]f32{0} ** 16 };
var WORLD_TRANSPOSE: Atomic(u32) = .init(0);
var WORLD_VALID: Atomic(bool) = .init(false);

/// G-buffers by framebuffer id: attachments and formats, probed once when the
/// target is first bound (`captureGbuffer`); the draw thunk only ever reads
/// this map, it never queries the framebuffer it is bound to.
var GBUFFERS: Mutex(fx.HashMap(u32, Gbuffer)) = .{ .value = .empty };
/// Framebuffers already probed and found NOT to be a G-buffer (shadow maps,
/// tonemap targets, ...), so a pass that binds one does not re-query its
/// attachments on every draw.
var NOT_GBUFFERS: Mutex(fx.HashSet(u32)) = .{ .value = .empty };
/// Every bloom/shadow/feed target is probed once; at 32 a busy frame cycled the
/// list and re-probed (4+ driver queries) on every bind.
const NOT_GBUFFER_LIMIT: usize = 1024;
/// A composite that cannot be built (or whose target cannot take it) disables
/// the FLIR view: the game's own image is then kept (fail closed).
var COMPOSITE_FAILED: Atomic(bool) = .init(false);

// View identity is keyed by GL objects and by GL command order, never by the
// scene-submit sequence. The submit hook runs before a view draws anything, but
// the draws of different views interleave, so no "current submit" state can
// bracket GL draws reliably. What is tracked instead:
//   * `CURRENT_FBO` - the draw framebuffer the game has bound right now
//     (`glBindFramebuffer` thunk), so every draw is attributed to a target
//     without a driver query.
//   * `TONEMAP_NEXT` - the IR decision of the `0x68B8C0` uniforms pass that just
//     ran, consumed by the tonemap quad it configures (same GL thread, GL order).
//   * `TARGETS` - tonemap output target -> the IR state of its last quad, the
//     G-buffer that quad was rendered from (learned at the quad, where both GL
//     objects are visible), and whether the target is the main view (learned
//     from the first framebuffer bind after a quad: FBO 0 = the main view, since
//     FXAA presents into the default framebuffer).

/// One tonemap output target: `ir` is the IR state of its last tonemap quad
/// (decided by that quad's own `0x68B8C0` params), `main` whether its first
/// framebuffer bind after a quad was FBO 0, `camera` the IR-latched camera an IR
/// view takes its config/ambient from. `seen_ms` refreshes on every quad. A feed
/// target is `main = false`, which is what keeps F10 out of camera feeds.
const TargetEntry = struct {
    ir: bool,
    main: bool,
    camera: usize,
    seen_ms: u64,
};

const Targets = struct {
    by_fbo: fx.HashMap(u32, TargetEntry) = .empty,
    /// G-buffer framebuffer id -> tonemap target rendered from it.
    gbuf_target: fx.HashMap(u32, u32) = .empty,
    /// Tonemap target -> its G-buffer (the reverse lookup the composite needs).
    target_gbuf: fx.HashMap(u32, u32) = .empty,

    /// Drop targets no tonemap quad refreshed for `FLIR_PAIRING_STALE_MS`, and
    /// the pairings that belong to them. A dropped feed target then has no
    /// pairing, so nothing is composited into it (nor substituted in its
    /// G-buffer) until its next quad re-learns it: a recycled framebuffer id is
    /// never trusted.
    fn prune(self: *Targets, now: u64) void {
        var stale: std.ArrayList(u32) = .empty;
        defer stale.deinit(gpa);
        var it = self.by_fbo.iterator();
        while (it.next()) |e| {
            if (now -| e.value_ptr.seen_ms > FLIR_PAIRING_STALE_MS) stale.append(gpa, e.key_ptr.*) catch {};
        }
        for (stale.items) |target| {
            _ = self.by_fbo.remove(target);
            self.forgetPairing(target);
        }
    }

    fn forgetPairing(self: *Targets, target: u32) void {
        if (self.target_gbuf.fetchRemove(target)) |kv| {
            if (self.gbuf_target.get(kv.value) == target) _ = self.gbuf_target.remove(kv.value);
        }
    }
};

var TARGETS: Mutex(Targets) = .{ .value = .{} };
const TARGET_LIMIT: usize = 8;
/// A camera feed renders only every few seconds (the monitor refresh), and its
/// G-buffer pass runs BEFORE the tonemap quad that learns the pairing. The
/// pairing therefore has to outlive the gap between two feed renders (the
/// shared 1 s `FLIR_TARGET_STALE_MS` pruned it every time, so a feed's G-buffer
/// pass never saw a pairing and was never substituted). Whether the view is
/// still FLIR is re-checked live against the camera's enable latch (see
/// `updateActiveView`), so a long lifetime cannot keep a disabled view hot.
const FLIR_PAIRING_STALE_MS: u64 = 30_000;
/// Draw framebuffer currently bound, in GL command order.
var CURRENT_FBO: Atomic(u32) = .init(0);
/// Newest G-buffer-like framebuffer bound since the last tonemap quad: at the
/// quad that is the view's G-buffer.
var LAST_GBUF: Atomic(u32) = .init(0);
/// The tonemap target that drew last, held until the first framebuffer bind after
/// its quad. That bind identifies the view: FBO 0 for the main view (FXAA
/// presents into the default framebuffer), another target for a camera feed.
var PENDING_QUAD_TARGET: Atomic(u32) = .init(0);
/// IR decision of the last `0x68B8C0` call, for the tonemap quad that follows it
/// in GL order: 0 = no pending uniforms pass, 1 = pending and not IR-graded,
/// 2 = pending and IR-graded. Swapped to 0 by the quad draw, so only the real
/// tonemap quad learns a view - not the other passes that use the same shared
/// `0x68BB00` quad helper.
var TONEMAP_NEXT: Atomic(u32) = .init(0);
/// FLIR-enabled camera of the pending tonemap quad (valid when `TONEMAP_NEXT == 2`).
var TONEMAP_CAMERA: Atomic(usize) = .init(0);
/// False when the 0x68B8C0 hook is not installed (marker or prologue mismatch):
/// every matched quad draw is then treated as a tonemap quad.
var TONEMAP_HOOK_OK: Atomic(bool) = .init(false);
/// Lock-free mirror of the G-buffers whose paired target is IR/F10-forced, so
/// the per-draw gate needs no lock (see `refreshForcedGbuffers`).
const FORCED_GBUF_SLOTS: usize = 4;
var FORCED_GBUFS: [FORCED_GBUF_SLOTS]Atomic(u32) = [_]Atomic(u32){.init(0)} ** FORCED_GBUF_SLOTS;
/// Per forced G-buffer slot: the FLIR camera whose enable latch must still be
/// live (0 = the F10 main view, which has no camera), and whether it is the main view.
var FORCED_CAMS: [FORCED_GBUF_SLOTS]Atomic(usize) = [_]Atomic(usize){.init(0)} ** FORCED_GBUF_SLOTS;
var FORCED_MAIN: [FORCED_GBUF_SLOTS]Atomic(bool) = [_]Atomic(bool){.init(false)} ** FORCED_GBUF_SLOTS;
/// View kind the draw framebuffer currently bound belongs to for the block pass:
/// 0 = not substituted, 1 = main view (F10), 2 = camera feed with FLIR enabled.
/// Recomputed at every framebuffer bind, so the per-draw gate needs no lock.
var ACTIVE_VIEW: Atomic(u32) = .init(0);
/// Mirror of "some target is currently IR-graded": gates matrix capture.
var IR_WANTED: Atomic(bool) = .init(false);

/// Bumped per scene submit: the body-volume upload throttle's clock. No rendering
/// decision uses it (see `TARGETS`).
var FRAME: Atomic(u64) = .init(0);
var MAP_REBUILD_MS: Atomic(u64) = .init(0);

/// One-shot keys for diagnostics that need a value-dependent key (view size,
/// framebuffer id), unlike `logging.logOnce`'s fixed bits. Capped so a game
/// that cycles targets cannot grow it without bound.
var DIAG_KEYS: Mutex(std.ArrayList(u64)) = .{ .value = .empty };
const DIAG_KEY_LIMIT: usize = 24;

fn logOnceKey(key: u64, comptime fmt: []const u8, args: anytype) void {
    if (!verbose) return;
    {
        const keys = DIAG_KEYS.lock();
        defer DIAG_KEYS.unlock();
        // 0x6a5_xxxx: a handful of fixed feature keys, never starved by the
        // per-object keys filling the cap.
        if ((keys.items.len >= DIAG_KEY_LIMIT and key >> 16 != 0x6a5) or std.mem.indexOfScalar(u64, keys.items, key) != null) return;
        keys.append(gpa, key) catch return;
    }
    logf(fmt, args);
}
var F10_ON: Atomic(bool) = .init(false);
var KEY_HELD: Atomic(bool) = .init(false);

/// Cameras whose IR latch was seen set, newest tick wins. Each one keeps the
/// ambient temperature last sampled at its own pivot, so a view reads its own
/// camera's ambient instead of whichever camera ticked last.
const CAMERA_SLOTS: usize = 4;

const CameraSlot = struct {
    pointer: Atomic(usize) = .init(0),
    ambient_bits: Atomic(u32) = .init(0),
    ambient_valid: Atomic(bool) = .init(false),
    tick_ms: Atomic(u64) = .init(0),
};

var CAMERAS: [CAMERA_SLOTS]CameraSlot = [_]CameraSlot{.{}} ** CAMERA_SLOTS;

// One-shot diagnostic keys (verbose builds only, see `logging.logOnce`).
// `flir` owns bits 8..22 of the shared one-shot word; `blocktemp` owns 0..8.
// Every message owns its own bit: a shared bit silently drops everything after
// the first message that used it.
const LOG_FLIR_TONEMAP: u32 = 1 << 8;
const LOG_FLIR_MESH_MAP: u32 = 1 << 9;
const LOG_FLIR_BLOCK: u32 = 1 << 10;
const LOG_FLIR_NO_ATTRIB: u32 = 1 << 11;
const LOG_FLIR_GRID_MISMATCH: u32 = 1 << 12;
const LOG_FLIR_MESH_MISS: u32 = 1 << 13;
const LOG_FLIR_VP: u32 = 1 << 14;
const LOG_FLIR_VP_MISS: u32 = 1 << 15;
const LOG_FLIR_SUBMIT: u32 = 1 << 19;
const LOG_FLIR_CAMERA: u32 = 1 << 21;
const LOG_FLIR_SHADER: u32 = 1 << 22;

const DIAG_SUBSTITUTED: usize = 0;
const DIAG_UNMAPPED: usize = 1;
const DIAG_NO_GRID: usize = 2;
const DIAG_COMPONENT: usize = 3;
/// Per view kind (0 = main/F10, 1 = camera feed) block-pass counters since the
/// last report: substituted draws, draws whose IBO is not in the mesh->body map,
/// draws of a mapped body that has no grid texture yet (drawn by the game, i.e.
/// at ambient). Only counted/reported in verbose builds.
var DIAG_COUNTS: [2][4]Atomic(u32) = [_][4]Atomic(u32){[_]Atomic(u32){.init(0)} ** 4} ** 2;
var DIAG_LAST_MS: Atomic(u64) = .init(0);

fn diagCount(which: usize) void {
    if (!verbose) return;
    const view = ACTIVE_VIEW.load(.monotonic);
    if (view != 0) _ = DIAG_COUNTS[view - 1][which].fetchAdd(1, .monotonic);
}

/// One line per ~5 s while any view is FLIR-active (called at every tonemap quad).
fn diagTick(now: u64) void {
    if (!verbose) return;
    if (!IR_WANTED.load(.monotonic) and !F10_ON.load(.monotonic)) return;
    const last = DIAG_LAST_MS.load(.monotonic);
    if (now -| last < 5000) return;
    DIAG_LAST_MS.store(now, .monotonic);
    if (last == 0) return;
    const secs = util.f(now - last) / 1000.0;
    var counts: [2][4]f64 = undefined;
    for (0..2) |view| {
        for (0..4) |i| counts[view][i] = util.f(DIAG_COUNTS[view][i].swap(0, .monotonic)) / secs;
    }
    const entries = blk: {
        const map = MESH_BODY.lock();
        defer MESH_BODY.unlock();
        break :blk map.count();
    };
    const main = counts[0];
    const feed = counts[1];
    logf("FLIR counters per s over {d:.1} s (map {d} ibos): main substituted={d:.1} unmapped_ibo={d:.1} no_grid_yet={d:.1} component={d:.1} | feed substituted={d:.1} unmapped_ibo={d:.1} no_grid_yet={d:.1} component={d:.1}", .{
        secs, entries, main[0], main[1], main[2], main[3], feed[0], feed[1], feed[2], feed[3],
    });
    const queries = GL_QUERIES.swap(0, .monotonic);
    const hook_us = HOOK_US.swap(0, .monotonic);
    const sub_us = HOOK_SUB_US.swap(0, .monotonic);
    var sites_text: std.ArrayList(u8) = .empty;
    defer sites_text.deinit(gpa);
    {
        const sites = GLQ_SITES.lock();
        defer GLQ_SITES.unlock();
        std.mem.sort(QuerySite, sites.items, {}, struct {
            fn lessThan(_: void, a: QuerySite, b: QuerySite) bool {
                return a.count > b.count;
            }
        }.lessThan);
        sites_text.append(gpa, '[') catch {};
        for (sites.items[0..@min(sites.items.len, 6)], 0..) |s, i| {
            if (i != 0) sites_text.appendSlice(gpa, ", ") catch {};
            sites_text.print(gpa, "({d}, {d})", .{ s.line, s.count }) catch {};
        }
        sites_text.append(gpa, ']') catch {};
        sites.clearRetainingCapacity();
    }
    logf("FLIR perf per s: gl_queries={d:.1} draw_thunk_own_ms={d:.2} (substituted draws {d:.2}) top query sites (src/flir.zig line, count): {s}", .{
        util.f(queries) / secs, util.f(hook_us) / 1000.0 / secs, util.f(sub_us) / 1000.0 / secs, sites_text.items,
    });
    const rows = [_]struct { label: []const u8, row: usize }{ .{ .label = "map-hit", .row = 1 }, .{ .label = "component/unmapped", .row = 0 } };
    for (rows) |r| {
        var parts: std.ArrayList(u8) = .empty;
        defer parts.deinit(gpa);
        for (0..SK_N) |i| {
            if (i != 0) parts.append(gpa, ' ') catch {};
            parts.print(gpa, "{s}={d:.1}", .{ SK_NAMES[i], util.f(SKIP_COUNTS[r.row][i].swap(0, .monotonic)) / secs }) catch {};
        }
        logf("FLIR counters main-view per s, {s} ibos: {s}", .{ r.label, parts.items });
    }
}

/// Diagnostic that is not one-shot (a state transition). Verbose builds only:
/// wiring lines stay out of the player build, which logs only prologue and
/// hook-install failures.
inline fn logDebug(comptime fmt: []const u8, args: anytype) void {
    if (verbose) logf(fmt, args);
}

// ---------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------

/// Marker file next to the DLL (`<plugin>/infrared_disable_*`).
fn markerExists(name: []const u8) bool {
    const dir = state.PLUGIN_DIR orelse return false;
    return dir.join(name).exists();
}

/// Install the FLIR hooks and GL dispatch slots. Each piece is independent and
/// non-fatal: a mismatch disables only that piece, with one log line.
///
/// Per-hook marker files next to the DLL (each disables ONE hook, so a
/// rendering problem can be bisected without a rebuild):
/// `infrared_disable_flir_tonemap` (0x68B8C0),
/// `infrared_disable_flir_draw` (glDrawElements slot),
/// `infrared_disable_flir_matrix` (glUniformMatrix4fv slot),
/// `infrared_disable_flir_fbo` (glBindFramebuffer slot).
pub fn install(base: usize) void {
    if (state.FLIR_DISABLED.load(.seq_cst)) {
        logLine("FLIR hooks disabled by request");
        return;
    }
    const tonemap_off = markerExists("infrared_disable_flir_tonemap");
    DRAW_THUNK_DISABLED.store(markerExists("infrared_disable_flir_draw"), .seq_cst);
    MATRIX_THUNK_DISABLED.store(markerExists("infrared_disable_flir_matrix"), .seq_cst);
    FBO_THUNK_DISABLED.store(markerExists("infrared_disable_flir_fbo"), .seq_cst);
    const draw_off = DRAW_THUNK_DISABLED.load(.seq_cst);
    const matrix_off = MATRIX_THUNK_DISABLED.load(.seq_cst);
    const fbo_off = FBO_THUNK_DISABLED.load(.seq_cst);
    if (tonemap_off or draw_off or matrix_off or fbo_off) {
        logf("FLIR per-hook switches: tonemap={d} draw={d} matrix={d} fbo={d} (the disabled hook(s) are NOT installed)", .{
            @intFromBool(tonemap_off), @intFromBool(draw_off), @intFromBool(matrix_off), @intFromBool(fbo_off),
        });
    }
    if (!tonemap_off) {
        installCodeHook(base, offsets.RVA_TONEMAP, &offsets.TONEMAP_PROLOGUE, "tonemap pass", &irTonemapHook, &state.ORIG_TONEMAP);
        // The quad hint is only meaningful when the detour is installed; the
        // draw thunk falls back to "every matched quad" without it.
        TONEMAP_HOOK_OK.store(state.ORIG_TONEMAP.load(.seq_cst) != 0, .seq_cst);
    }

    GL_BASE.store(base, .seq_cst);
    ensureGlSlots();
}

const SlotPatch = struct { rva: usize, ours: usize, original: *Atomic(usize), label: []const u8, disabled: bool };

/// (Re-)point the GL dispatch slots at our thunks. Safe to call twice: a slot
/// that already forwards to us has a non-zero original and is skipped.
fn ensureGlSlots() void {
    const base = GL_BASE.load(.seq_cst);
    if (base == 0) return;
    const slots = [_]SlotPatch{
        .{ .rva = offsets.RVA_GL_DRAW_ELEMENTS, .ours = @intFromPtr(&swi_gl_draw_elements_entry), .original = &state.ORIG_GL_DRAW_ELEMENTS, .label = "glDrawElements", .disabled = DRAW_THUNK_DISABLED.load(.seq_cst) },
        .{ .rva = offsets.RVA_GL_UNIFORM_MATRIX4FV, .ours = @intFromPtr(&glUniformMatrix4fvHook), .original = &state.ORIG_GL_UNIFORM_MATRIX4FV, .label = "glUniformMatrix4fv", .disabled = MATRIX_THUNK_DISABLED.load(.seq_cst) },
        .{ .rva = offsets.RVA_GL_BIND_FRAMEBUFFER, .ours = @intFromPtr(&glBindFramebufferHook), .original = &state.ORIG_GL_BIND_FRAMEBUFFER, .label = "glBindFramebuffer", .disabled = FBO_THUNK_DISABLED.load(.seq_cst) },
        .{ .rva = offsets.RVA_GL_USE_PROGRAM, .ours = @intFromPtr(&glUseProgramHook), .original = &ORIG_GL_USE_PROGRAM, .label = "glUseProgram", .disabled = false },
        .{ .rva = offsets.RVA_GL_BIND_BUFFER, .ours = @intFromPtr(&glBindBufferHook), .original = &ORIG_GL_BIND_BUFFER, .label = "glBindBuffer", .disabled = false },
        .{ .rva = offsets.RVA_GL_ACTIVE_TEXTURE, .ours = @intFromPtr(&glActiveTextureHook), .original = &ORIG_GL_ACTIVE_TEXTURE, .label = "glActiveTexture", .disabled = false },
        .{ .rva = offsets.RVA_GL_BIND_VERTEX_ARRAY, .ours = @intFromPtr(&glBindVertexArrayHook), .original = &ORIG_GL_BIND_VERTEX_ARRAY, .label = "glBindVertexArray", .disabled = false },
        .{ .rva = offsets.RVA_GL_UNIFORM1F, .ours = @intFromPtr(&glUniform1fHook), .original = &ORIG_GL_UNIFORM1F, .label = "glUniform1f", .disabled = false },
        .{ .rva = offsets.RVA_GL_UNIFORM3F, .ours = @intFromPtr(&glUniform3fHook), .original = &ORIG_GL_UNIFORM3F, .label = "glUniform3f", .disabled = false },
        .{ .rva = offsets.RVA_GL_BIND_TEXTURE, .ours = @intFromPtr(&glBindTextureHook), .original = &ORIG_GL_BIND_TEXTURE, .label = "glBindTexture", .disabled = false },
        .{ .rva = offsets.RVA_GL_VIEWPORT, .ours = @intFromPtr(&glViewportHook), .original = &ORIG_GL_VIEWPORT, .label = "glViewport", .disabled = false },
    };
    for (slots) |s| {
        if (s.disabled) continue;
        if (s.original.load(.seq_cst) == 0) patchGlSlot(base + s.rva, s.ours, s.original, s.label);
    }
}

fn installCodeHook(base: usize, rva: usize, prologue: []const u8, label: []const u8, detour: *const anyopaque, original: *Atomic(usize)) void {
    const address = base + rva;
    if (!memory.bytesMatch(address, prologue)) {
        logf("FLIR {s} prologue mismatch at 0x{x}; FLIR {s} hook skipped (this build is not {s})", .{ label, address, label, offsets.GAME_BUILD_LABEL });
        return;
    }
    var status: minhook.Status = undefined;
    if (minhook.createHook(address, detour, &status)) |trampoline| {
        original.store(trampoline, .seq_cst);
        if (!minhook.enableHook(address, &status)) {
            logf("FLIR: MinHook enable_hook({s}) failed: {f}", .{ label, status });
        } else {
            logDebug("FLIR {s} hook installed at 0x{x}", .{ label, address });
        }
    } else {
        logf("FLIR: MinHook create_hook({s}) failed: {f}", .{ label, status });
    }
}

/// Re-point a GL dispatch slot at `ours`, keeping the previous pointer as the
/// forwarding target. Both known slots (`0x140A935F8` IAT entry,
/// `0x140CE3268`/`0x140CE5688` GLEW globals) are plain pointer variables, so the
/// slot itself is the target. Every read is validated and the value is only
/// accepted when it points into a loaded module, so a wrong RVA or an
/// uninitialised slot cannot be published as the forwarding target.
fn patchGlSlot(slot: usize, ours: usize, original: *Atomic(usize), label: []const u8) void {
    const target = slot;
    if (!readable(target, @sizeOf(usize))) {
        logDebug("FLIR: GL slot {s} target 0x{x} is not mapped; {s} hook skipped", .{ label, target, label });
        return;
    }
    const previous = memory.peek(usize, target);
    if (previous == ours) return;
    if (!memory.imageBacked(previous)) {
        logDebug("FLIR: GL slot {s} at 0x{x} does not hold a module pointer (value 0x{x}); {s} hook skipped", .{ label, slot, previous, label });
        return;
    }
    var old_protect: u32 = 0;
    if (win.VirtualProtect(@ptrFromInt(target), @sizeOf(usize), win.PAGE_EXECUTE_READWRITE, &old_protect) == 0) {
        logDebug("FLIR: could not unprotect GL slot {s} at 0x{x}; {s} hook skipped", .{ label, target, label });
        return;
    }
    original.store(previous, .seq_cst);
    memory.poke(usize, target, ours);
    var ignored: u32 = 0;
    _ = win.VirtualProtect(@ptrFromInt(target), @sizeOf(usize), old_protect, &ignored);
    logDebug("FLIR: GL slot {s} hooked at 0x{x} (was 0x{x})", .{ label, target, previous });
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

/// Record `camera` in the slot registry: its own slot, else an empty one, else
/// the one that has been stale longest.
fn registerCamera(camera: usize, now: u64) void {
    var candidate: ?usize = null;
    var candidate_ms: u64 = std.math.maxInt(u64);
    for (&CAMERAS, 0..) |*slot, index| {
        const pointer = slot.pointer.load(.monotonic);
        if (pointer == camera) {
            slot.tick_ms.store(now, .monotonic);
            return;
        }
        if (pointer < 0x10000) {
            candidate = index;
            break;
        }
        const tick = slot.tick_ms.load(.monotonic);
        if (tick < candidate_ms) {
            candidate_ms = tick;
            candidate = index;
        }
    }
    const index = candidate orelse return;
    const slot = &CAMERAS[index];
    slot.pointer.store(camera, .monotonic);
    slot.tick_ms.store(now, .monotonic);
    slot.ambient_valid.store(false, .monotonic);
}

/// Ambient temperature last sampled at `camera`'s own pivot.
fn cameraAmbient(camera: usize) f32 {
    for (&CAMERAS) |*slot| {
        if (slot.pointer.load(.monotonic) == camera and slot.ambient_valid.load(.monotonic)) {
            return @bitCast(slot.ambient_bits.load(.monotonic));
        }
    }
    return offsets.FLIR_AMBIENT_FALLBACK_C;
}

/// Scene ambient air temperature cached by `sceneTick` on the game thread (the
/// median of the thermal sim's per-body ambient samples), or the fixed fallback
/// when the sim has not stepped a body yet. Used by the main view (F10), which has
/// no IR camera to sample at.
fn sceneAmbient() f32 {
    const value: f32 = @bitCast(state.FLIR_SCENE_AMBIENT_BITS.load(.monotonic));
    return if (std.math.isFinite(value)) value else offsets.FLIR_AMBIENT_FALLBACK_C;
}

// Camera/view discovery at the scene submit `0x6BB400`. The entry stub saves the
// caller's RBX/R15/RSI (callee-saved, still intact at function entry) and jumps to
// the detour with every argument register and the stack untouched. Per the
// RE: at the main submit RSI == arg11 (main view state) and R15 = main camera; at
// the feed submit RBX = the feed camera and arg11 = the feed view state.
var SUBMIT_RBX: usize = 0;
var SUBMIT_R15: usize = 0;
var SUBMIT_RSI: usize = 0;

comptime {
    @export(&SUBMIT_RBX, .{ .name = "swi_submit_rbx", .visibility = .hidden });
    @export(&SUBMIT_R15, .{ .name = "swi_submit_r15", .visibility = .hidden });
    @export(&SUBMIT_RSI, .{ .name = "swi_submit_rsi", .visibility = .hidden });
    @export(&hooks.irFrameUniformsHook, .{ .name = "swi_frame_uniforms_hook", .visibility = .hidden });
    asm (
        \\.text
        \\.globl swi_scene_submit_entry
        \\swi_scene_submit_entry:
        \\    movq %rbx, swi_submit_rbx(%rip)
        \\    movq %r15, swi_submit_r15(%rip)
        \\    movq %rsi, swi_submit_rsi(%rip)
        \\    jmp swi_frame_uniforms_hook
    );
}

extern fn swi_scene_submit_entry() callconv(.c) void;

/// Address of the scene-submit entry stub (the MinHook detour for `0x6BB400`).
pub fn sceneSubmitEntry() *const anyopaque {
    return @ptrCast(&swi_scene_submit_entry);
}

fn submitReg(reg: *const usize) usize {
    return @atomicLoad(usize, reg, .monotonic);
}

/// The old `cfg!(debug_assertions)` gate (F10 and other debug-only paths):
/// on for Debug and ReleaseSafe builds.
const DEBUG_ASSERTIONS = std.debug.runtime_safety;

/// Main view's camera (R15 at the main submit), 0 until seen.
var MAIN_CAMERA: Atomic(usize) = .init(0);
/// Persistent render-target holder W -> (camera, last seen ms). Never keyed by the
/// feed view state, which is `operator new`'d and freed every frame.
const HolderCamera = struct { camera: usize, seen_ms: u64 };
var HOLDER_CAMERA: Mutex(fx.HashMap(usize, HolderCamera)) = .{ .value = .empty };
const HOLDER_STALE_MS: u64 = 5000;
const HOLDER_LIMIT: usize = 64;

fn validCamera(camera: usize) bool {
    const base = GL_BASE.load(.monotonic);
    return base != 0 and camera >= PTR_MIN and
        camera % 8 == 0 and // register values are arbitrary integers; objects are 8-aligned
        readable(camera, 8) and readUsizeAt(camera) == base + offsets.RVA_CAMERA_VTABLE;
}

/// Each scene submit (0x6bb400) renders one view and tells us which camera it is
/// for. Also the frame clock (body-volume upload throttle). Block substitution
/// and composite are still gated by GL objects/order, never by submit order.
pub fn onSceneSubmit(_: usize, view_state: usize, secondary: u8) void {
    _ = FRAME.fetchAdd(1, .monotonic);
    if (secondary != 0) logOnce(LOG_FLIR_SUBMIT, "FLIR: editor/preview scene submit observed (arg 12 = 1)", .{});
    const rbx = submitReg(&SUBMIT_RBX);
    const r15 = submitReg(&SUBMIT_R15);
    const rsi = submitReg(&SUBMIT_RSI);
    if (view_state < PTR_MIN) return;
    if (rsi == view_state) {
        if (validCamera(r15)) MAIN_CAMERA.store(r15, .monotonic);
        return;
    }
    if (!validCamera(rbx) or !readable(view_state + offsets.VIEW_STATE_RT_HOLDER, 8)) return;
    const holder = readUsizeAt(view_state + offsets.VIEW_STATE_RT_HOLDER);
    if (holder < PTR_MIN) return;
    const now = state.nowMs();
    const map = HOLDER_CAMERA.lock();
    defer HOLDER_CAMERA.unlock();
    {
        var drop: std.ArrayList(usize) = .empty;
        defer drop.deinit(gpa);
        var it = map.iterator();
        while (it.next()) |e| {
            if (now -| e.value_ptr.seen_ms > HOLDER_STALE_MS) drop.append(gpa, e.key_ptr.*) catch {};
        }
        for (drop.items) |k| _ = map.remove(k);
    }
    if (map.count() >= HOLDER_LIMIT and !map.contains(holder)) map.clearRetainingCapacity();
    map.put(gpa, holder, .{ .camera = rbx, .seen_ms = now }) catch {};
}

/// Camera of the view whose tonemap params block is `params` (`view_state+0x870`):
/// null holder = main view (its submit camera), else the holder's mapped camera.
fn cameraForParams(params: usize) usize {
    if (params < offsets.VIEW_STATE_TONEMAP_PARAMS) return 0;
    const view_state = params - offsets.VIEW_STATE_TONEMAP_PARAMS;
    if (view_state < PTR_MIN or !readable(view_state + offsets.VIEW_STATE_RT_HOLDER, 8)) return 0;
    const holder = readUsizeAt(view_state + offsets.VIEW_STATE_RT_HOLDER);
    if (holder == 0) return MAIN_CAMERA.load(.monotonic);
    const now = state.nowMs();
    const map = HOLDER_CAMERA.lock();
    defer HOLDER_CAMERA.unlock();
    const entry = map.get(holder) orelse return 0;
    return if (now -| entry.seen_ms <= HOLDER_STALE_MS) entry.camera else 0;
}

/// Scene tick (game thread): the FLIR mesh -> body map, plus the ambient
/// temperature at each registered camera's own pivot, so the render thread
/// never calls into the heat model itself and every view can use its own
/// camera's ambient.
pub fn sceneTick(scene: usize) void {
    // Mesh index buffer -> body map for the FLIR block pass. The walk reads the
    // game's vehicle/body containers, and ONLY the game thread may do that: a
    // vehicle spawn rebuilds them while the render thread draws, which is a data
    // race (it crashed the game). Published under `MESH_BODY`; the render thread
    // only ever looks an index buffer up in the published map.
    if (scene >= PTR_MIN) {
        const now = state.nowMs();
        const since = now -| MAP_REBUILD_MS.load(.monotonic);
        // Only while some view can use it (a camera with ir.flir on, or F10):
        // otherwise the walk is pure overhead. The first tick after FLIR turns
        // on rebuilds immediately because the throttle has long expired.
        const wanted = IR_WANTED.load(.monotonic) or (DEBUG_ASSERTIONS and F10_ON.load(.monotonic));
        if (wanted and since > 250) {
            MAP_REBUILD_MS.store(now, .monotonic);
            fault.enter(stage.FLIR_MESH_MAP);
            defer fault.leave();
            rebuildMeshBody(scene);
        }
    }
    const now = state.nowMs();
    for (&CAMERAS) |*slot| {
        const camera = slot.pointer.load(.monotonic);
        if (camera < 0x10000 or now -| slot.tick_ms.load(.monotonic) > offsets.FLIR_IR_CAMERA_STALE_MS) continue;
        if (!readable(camera + offsets.CAMERA_PIVOT, 24)) continue;
        const p = [3]f64{
            readF64At(camera + offsets.CAMERA_PIVOT),
            readF64At(camera + offsets.CAMERA_PIVOT + 8),
            readF64At(camera + offsets.CAMERA_PIVOT + 16),
        };
        if (!(std.math.isFinite(p[0]) and std.math.isFinite(p[1]) and std.math.isFinite(p[2]))) continue;
        const ambient = heat.baseAmbientAt(p);
        if (std.math.isFinite(ambient)) {
            slot.ambient_bits.store(@bitCast(ambient), .monotonic);
            slot.ambient_valid.store(true, .monotonic);
        }
    }
    // A view with no IR camera of its own (the F10 main view) uses the scene's
    // ambient temperature. The heat evaluator is game-thread only, so cache a
    // value here: the median of the per-body ambient samples the sim just took.
    // The render thread only reads the atomic.
    if (blocktemp.medianBodyAmbient()) |ambient| state.FLIR_SCENE_AMBIENT_BITS.store(@bitCast(ambient), .monotonic);
    if (blocktemp.medianGroundAmbient()) |ground| GROUND_AMBIENT_BITS.store(@bitCast(ground), .monotonic);
    // Sun, time of day and haze for the composite (game-thread reads, cached
    // for the render thread).
    if (sun.gameSun()) |s| {
        {
            const g = GAME_SUN.lock();
            defer GAME_SUN.unlock();
            g.* = .{ @floatCast(s.dir[0]), @floatCast(s.dir[1]), @floatCast(s.dir[2]) };
        }
        DAY_PCT_BITS.store(@bitCast(@as(f32, @floatCast(s.percent))), .monotonic);
    }
    const weather = @import("ir.zig").weatherFogRain();
    // FlightGear scales its diurnal surface deltas by the clear-sky factor.
    const clear = std.math.clamp(1.0 - 0.8 * util.fmax(weather[0], weather[1]), 0.2, 1.0);
    CLEAR_SKY_BITS.store(@bitCast(clear), .monotonic);
}

/// Jet exhaust plumes per body (body-local metres): start, dir, radius, excess C.
const Plume = blocktemp.Plume;
const BodyPlumes = struct { body: usize, seen_ms: u64, plumes: std.ArrayList(Plume) };
var PLUMES: Mutex(std.ArrayList(BodyPlumes)) = .{ .value = .empty };
const MAX_PLUMES: usize = 8;

/// Game thread (blocktemp body step): replace a body's plumes.
pub fn setBodyPlumes(body: usize, plumes: []const Plume) void {
    const now = state.nowMs();
    const all = PLUMES.lock();
    defer PLUMES.unlock();
    var keep: usize = 0;
    for (all.items) |*e| {
        if (e.body != body and now -| e.seen_ms < 2_000) {
            all.items[keep] = e.*;
            keep += 1;
        } else {
            e.plumes.deinit(gpa);
        }
    }
    all.shrinkRetainingCapacity(keep);
    if (plumes.len > 0 and all.items.len < 64) {
        var copy: std.ArrayList(Plume) = .empty;
        copy.appendSlice(gpa, plumes) catch return;
        all.append(gpa, .{ .body = body, .seen_ms = now, .plumes = copy }) catch copy.deinit(gpa);
    }
}

/// This view's plumes in render space, packed for the composite:
/// a = (start, excess), b = (dir, radius).
const ViewPlumes = struct { a: [MAX_PLUMES][4]f32 = undefined, b: [MAX_PLUMES][4]f32 = undefined, n: usize = 0 };

/// Render thread: this view's plumes in render space.
fn viewPlumes() ViewPlumes {
    var out: ViewPlumes = .{};
    const all = PLUMES.lock();
    defer PLUMES.unlock();
    if (all.items.len == 0) return out;
    const view = ACTIVE_VIEW.load(.monotonic);
    const mats = BODY_MATS.lock();
    defer BODY_MATS.unlock();
    for (all.items) |entry| {
        var best: ?*const BodyMat = null;
        for (mats.items) |*e| {
            if (e.view == view and e.body == entry.body and (best == null or e.frame > best.?.frame)) best = e;
        }
        const m = (best orelse continue).mat;
        for (entry.plumes.items) |pl| {
            if (out.n >= MAX_PLUMES) break;
            const p = pl.start;
            const d = pl.dir;
            const w = [3]f32{
                m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12],
                m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13],
                m[2] * p[0] + m[6] * p[1] + m[10] * p[2] + m[14],
            };
            const wd = [3]f32{
                m[0] * d[0] + m[4] * d[1] + m[8] * d[2],
                m[1] * d[0] + m[5] * d[1] + m[9] * d[2],
                m[2] * d[0] + m[6] * d[1] + m[10] * d[2],
            };
            const n = @sqrt(wd[0] * wd[0] + wd[1] * wd[1] + wd[2] * wd[2]);
            if (!(n > 1e-6) or !(std.math.isFinite(w[0]) and std.math.isFinite(w[1]) and std.math.isFinite(w[2]))) continue;
            out.a[out.n] = .{ w[0], w[1], w[2], pl.excess_c };
            out.b[out.n] = .{ wd[0] / n, wd[1] / n, wd[2] / n, pl.radius };
            out.n += 1;
        }
    }
    return out;
}

/// Sea-level air under the vehicles: terrain/water ambient (NaN until known).
var GROUND_AMBIENT_BITS: Atomic(u32) = .init(0x7fc0_0000);
/// To-sun unit vector from the game's clock-driven sun (scene tick).
var GAME_SUN: Mutex(?[3]f32) = .{ .value = null };
/// Day fraction (0 = midnight, 0.5 = noon) and clear-sky factor (0.2..1).
var DAY_PCT_BITS: Atomic(u32) = .init(0x3f00_0000);
var CLEAR_SKY_BITS: Atomic(u32) = .init(0x3f80_0000);

/// Tonemap uniforms `0x68b8c0` (verified: uniforms only, no draw). `params` is
/// `view_state+0x870` (`r8`). It runs immediately before the view's tonemap quad
/// in GL order: it resolves the view's camera and, when `ir.flir(true)` is fresh for
/// that camera, marks the next `0x68BB00` quad for the FLIR composite.
fn irTonemapHook(ctx: usize, view: usize, params: usize) callconv(.c) void {
    const original = state.ORIG_TONEMAP.load(.seq_cst);
    const camera: usize = blk: {
        const c = cameraForParams(params);
        if (c >= PTR_MIN and state.flirEnabledCfg(c) != null) {
            registerCamera(c, state.nowMs());
            break :blk c;
        }
        break :blk 0;
    };
    // Publish before forwarding: the quad draw follows on this thread immediately.
    TONEMAP_CAMERA.store(camera, .monotonic);
    TONEMAP_NEXT.store(if (camera != 0) 2 else 1, .release);
    if (original != 0) {
        const f: *const fn (usize, usize, usize) callconv(.c) void = @ptrFromInt(original);
        f(ctx, view, params);
    }
    if (camera != 0) logOnce(LOG_FLIR_TONEMAP, "FLIR: tonemap pass for FLIR-enabled camera 0x{x}; the quad that follows is composited", .{camera});
}

// ---------------------------------------------------------------------------
// Target registry (view identity by GL object)
// ---------------------------------------------------------------------------

/// Recompute the lock-free mirrors from `targets` (caller holds the lock).
fn publishTargets(targets: *const Targets, f10: bool) void {
    var any_ir = false;
    var values = targets.by_fbo.valueIterator();
    while (values.next()) |entry| {
        if (entry.ir) any_ir = true;
    }
    IR_WANTED.store(any_ir, .monotonic);
    const Forced = struct { gbuffer: u32, camera: usize, main: bool };
    var forced: [FORCED_GBUF_SLOTS]Forced = undefined;
    var n: usize = 0;
    var it = targets.gbuf_target.iterator();
    while (it.next()) |pair| {
        const gbuffer = pair.key_ptr.*;
        const target = pair.value_ptr.*;
        // Only a G-buffer paired with a target whose last tonemap quad was
        // IR-graded (or, in a debug build, the main view while F10 is on) is
        // substituted in. A feed target is never forced by F10; an unpaired
        // G-buffer is never touched at all.
        const entry = targets.by_fbo.get(target) orelse continue;
        var camera: usize = 0;
        var main = false;
        if (entry.ir) {
            camera = entry.camera;
            main = entry.main;
        } else if (f10 and entry.main) {
            main = true;
        } else continue;
        logOnceKey(0x06a5_e000 | ((@as(u64, gbuffer) & 0x7f) << 7) | (@as(u64, target) & 0x7f), "FLIR: G-buffer {d} of target {d} now substituted (main={}, camera=0x{x})", .{ gbuffer, target, main, camera });
        if (n < FORCED_GBUF_SLOTS) {
            forced[n] = .{ .gbuffer = gbuffer, .camera = camera, .main = main };
            n += 1;
        }
    }
    for (&FORCED_GBUFS, 0..) |*slot, index| {
        const f: Forced = if (index < n) forced[index] else .{ .gbuffer = 0, .camera = 0, .main = false };
        FORCED_CAMS[index].store(f.camera, .monotonic);
        FORCED_MAIN[index].store(f.main, .monotonic);
        slot.store(f.gbuffer, .monotonic);
    }
}

/// Decide, for the draw framebuffer that was just bound, whether the block pass
/// substitutes into it: it must be a paired G-buffer of an IR-graded target whose
/// camera's FLIR latch is STILL on (checked here, once per bind, not per draw), or
/// the F10 main view. A feed is judged by its own camera, never by the main view.
fn updateActiveView(framebuffer: u32) void {
    var kind: u32 = 0;
    if (framebuffer != 0) {
        for (&FORCED_GBUFS, 0..) |*slot, index| {
            if (slot.load(.monotonic) != framebuffer) continue;
            const camera = FORCED_CAMS[index].load(.monotonic);
            if (camera == 0 or state.flirEnabledCfg(camera) != null) {
                kind = if (FORCED_MAIN[index].load(.monotonic)) 1 else 2;
            }
            break;
        }
    }
    ACTIVE_VIEW.store(kind, .monotonic);
}

/// Refresh the mirrors against the live registry (F10 can change at any time).
fn refreshForcedGbuffers() void {
    const f10 = DEBUG_ASSERTIONS and f10Toggled();
    const now = state.nowMs();
    {
        const targets = TARGETS.lock();
        defer TARGETS.unlock();
        targets.prune(now);
        publishTargets(targets, f10);
    }
    updateActiveView(CURRENT_FBO.load(.monotonic));
}

/// Set by the game thread when the scene object changes (world load/unload).
pub var RENDER_TARGETS_STALE: Atomic(bool) = .init(false);

/// Drop the probed G-buffer descriptions (positive and negative). GL recycles
/// framebuffer/texture ids when the game rebuilds its render targets (menu ->
/// world -> menu), so a cached id can describe a deleted target: the in-world
/// view then samples the menu's textures (flat grey) or a new G-buffer whose id
/// was once rejected is never captured. Rebuilt by the next binds.
fn forgetGbufferProbes(reason: []const u8) void {
    {
        const g = GBUFFERS.lock();
        defer GBUFFERS.unlock();
        g.clearRetainingCapacity();
    }
    {
        const n = NOT_GBUFFERS.lock();
        defer NOT_GBUFFERS.unlock();
        n.clearRetainingCapacity();
    }
    logDebug("FLIR: G-buffer probes dropped ({s})", .{reason});
}

/// Learn what a tonemap quad says about its view: the target's own IR state (the
/// `0x68B8C0` params of that quad) and, when the G-buffer bound before it is a real
/// one, the G-buffer/target pairing. This is the only place a view's G-buffer is
/// identified: GL order is the authority, not the submit order. Returns whether the
/// target is already known as the main view. One log line per learned pairing.
fn noteQuad(gbuffer: u32, target: u32, ir: bool, camera: usize, now: u64) bool {
    if (RENDER_TARGETS_STALE.swap(false, .monotonic)) forgetGbufferProbes("scene changed");
    const f10 = DEBUG_ASSERTIONS and f10Toggled();
    var new_target = false;
    var previous: ?u32 = null;
    var main = false;
    {
        const targets = TARGETS.lock();
        defer TARGETS.unlock();
        targets.prune(now);
        if (targets.by_fbo.count() >= TARGET_LIMIT and !targets.by_fbo.contains(target)) {
            // A long session can cycle targets; keep the registry small rather than
            // letting it grow (a cleared target is relearned from its next quad).
            targets.by_fbo.clearRetainingCapacity();
            targets.gbuf_target.clearRetainingCapacity();
            targets.target_gbuf.clearRetainingCapacity();
        }
        if (targets.by_fbo.getPtr(target)) |entry| {
            const was_ir = entry.ir;
            entry.ir = ir;
            entry.seen_ms = now;
            if (ir and camera >= PTR_MIN) {
                entry.camera = camera;
            } else if (!ir) {
                entry.camera = 0;
            }
            // A quad into the default framebuffer IS the main view; no bind
            // follows it to learn from.
            if (target == 0) entry.main = true;
            if (was_ir != ir) logDebug("FLIR: tonemap target {d} is now IR-graded={d}", .{ target, @intFromBool(ir) });
        } else {
            new_target = true;
            targets.by_fbo.put(gpa, target, .{ .ir = ir, .main = target == 0, .camera = if (ir) camera else 0, .seen_ms = now }) catch {};
            logDebug("FLIR: tonemap target {d} learned at its tonemap quad (IR={d})", .{ target, @intFromBool(ir) });
        }

        if (gbuffer != 0 and gbuffer != target) {
            if (targets.gbuf_target.count() >= TARGET_LIMIT and !targets.gbuf_target.contains(gbuffer) and !targets.target_gbuf.contains(target)) {
                // A game that cycles render targets must not grow the pairing either;
                // both sides are relearned at the next quad.
                targets.gbuf_target.clearRetainingCapacity();
                targets.target_gbuf.clearRetainingCapacity();
            }
            previous = if (targets.gbuf_target.fetchPut(gpa, gbuffer, target) catch null) |kv| kv.value else null;
            // A target rendered from another G-buffer now: drop the old pairing, so a
            // stale G-buffer id can never claim the target (or be substituted in).
            if (targets.target_gbuf.fetchPut(gpa, target, gbuffer) catch null) |kv| {
                const old_gbuffer = kv.value;
                if (old_gbuffer != gbuffer and targets.gbuf_target.get(old_gbuffer) == target) _ = targets.gbuf_target.remove(old_gbuffer);
            }
            if (previous) |old_target| {
                if (old_target != target and targets.target_gbuf.get(old_target) == gbuffer) _ = targets.target_gbuf.remove(old_target);
            }
        }
        main = if (targets.by_fbo.get(target)) |entry| entry.main else false;
        publishTargets(targets, f10);
    }
    // A new tonemap target means the game (re)built its render targets.
    if (new_target) forgetGbufferProbes("new tonemap target");
    if (gbuffer == 0 or gbuffer == target or previous == target) return main;
    // Keyed by the pair, so a target that alternates between G-buffers (or a
    // recycled id) cannot turn this into a per-frame line.
    const key = 0x06a5_c000 | ((@as(u64, gbuffer) & 0x7f) << 7) | (@as(u64, target) & 0x7f);
    logOnceKey(key, "FLIR: G-buffer {d} -> tonemap target {d} ({s} view, IR-graded={d}){s}", .{
        gbuffer, target, if (main) "main" else "feed/other", @intFromBool(ir), if (previous != null) ", changed" else "",
    });
    return main;
}

/// The first framebuffer bind after a tonemap quad identifies that view: the main
/// view presents into the default framebuffer (FBO 0, where FXAA draws), a camera
/// feed renders into an offscreen target. Learned per tonemap target; F10 only ever
/// forces a target learned as the main view. One log line per target/state.
fn noteFirstBind(target: u32, framebuffer: u32) void {
    const f10 = DEBUG_ASSERTIONS and f10Toggled();
    const now = state.nowMs();
    const main = framebuffer == 0;
    {
        const targets = TARGETS.lock();
        defer TARGETS.unlock();
        targets.prune(now);
        const entry = targets.by_fbo.getPtr(target) orelse return;
        if (main and !entry.main) {
            entry.main = true;
            publishTargets(targets, f10);
        }
    }
    const key = (@as(u64, 7) << 56) | ((@as(u64, target) & 0xffff) << 8) | @intFromBool(main);
    logOnceKey(key, "FLIR: tonemap target {d}: first bind after its quad is FBO {d} ({s})", .{
        target, framebuffer, if (main) "main view, F10 may force it" else "offscreen view (camera feed/other), F10 never forces it",
    });
}

/// The probed description of a G-buffer. No GL query: the bind thunk captured it
/// when the target was first bound.
fn cachedGbuffer(fbo: u32) ?Gbuffer {
    const g = GBUFFERS.lock();
    defer GBUFFERS.unlock();
    return g.get(fbo);
}

/// The G-buffer paired with a tonemap target (learned at the quad), with its
/// attachment info, if it is still known and usable.
fn pairedGbuffer(target: u32) ?Gbuffer {
    const gbuffer = blk: {
        const targets = TARGETS.lock();
        defer TARGETS.unlock();
        targets.prune(state.nowMs());
        break :blk targets.target_gbuf.get(target);
    } orelse return null;
    const entry = cachedGbuffer(gbuffer) orelse return null;
    if (entry.tex_normal == 0 or entry.tex_color == 0) return null;
    return entry;
}

/// The tonemap targets seen at a quad, for skip diagnostics (sorted, caller frees).
fn knownTargets() std.ArrayList(u32) {
    var known: std.ArrayList(u32) = .empty;
    const targets = TARGETS.lock();
    defer TARGETS.unlock();
    targets.prune(state.nowMs());
    var it = targets.by_fbo.keyIterator();
    while (it.next()) |k| known.append(gpa, k.*) catch break;
    std.mem.sort(u32, known.items, {}, std.sort.asc(u32));
    return known;
}

/// The arguments of the game's own tonemap quad draw.
const Quad = struct { mode: u32, count: i32, kind: u32, indices: ?*const anyopaque };

/// The tonemap quad draw (`0x68BB00`, recognised by its return address): the end
/// of a view's GL sequence. The last G-buffer bound before it and the framebuffer
/// it draws into are that view's pair, the target inherits this quad's own IR
/// state, and the quad is replaced with the composite when the target is IR-graded
/// (or F10 forces a target learned as the main view).
fn tonemapQuad(ir_graded: bool, camera: usize, quad: Quad) bool {
    const target = CURRENT_FBO.load(.monotonic);
    const gbuffer = LAST_GBUF.swap(0, .acq_rel);
    // The next framebuffer bind identifies this view (FBO 0 = main, see
    // `noteFirstBind`); a quad into target 0 already is the main view.
    if (target != 0) PENDING_QUAD_TARGET.store(target, .release);
    const now = state.nowMs();
    const main = noteQuad(gbuffer, target, ir_graded, camera, now);
    diagTick(now);
    if (main) dumpStep();
    // The next view uploads its own matrices; a view that never does must not
    // replay this one's transform.
    VIEW_PROJ_VALID.store(false, .monotonic);
    WORLD_VALID.store(false, .monotonic);
    if (ir_graded) return compositeView(target, ir_graded, false, main, camera, quad);
    // F10 (debug) forces only a target learned as the main view; a camera feed
    // renders FLIR only when its own tonemap params say IR.
    const forced = DEBUG_ASSERTIONS and main and f10Toggled();
    if (!forced) return false;
    return compositeView(target, ir_graded, forced, main, camera, quad);
}

/// Log once per distinct `(view size, target, IR state, F10 state, view kind)`.
/// Called at the tonemap quad draw, where the viewport is this view's own size.
fn logViewMode(viewport: [4]i32, target: u32, ir_graded: bool, forced: bool, main: bool) void {
    if (!verbose) return;
    const width = viewport[2];
    const height = viewport[3];
    const key = (@as(u64, 1) << 60) |
        ((@as(u64, @as(u32, @bitCast(width))) & 0xffff) << 44) |
        ((@as(u64, @as(u32, @bitCast(height))) & 0xffff) << 28) |
        ((@as(u64, target) & 0xffff) << 12) |
        (@as(u64, @intFromBool(ir_graded)) << 1) |
        @intFromBool(forced);
    logOnceKey(key, "FLIR: tonemap quad view {d}x{d} at ({d}, {d}) target {d} IR={d} F10={d} ({s} view)", .{
        width, height, viewport[0], viewport[1], target, @intFromBool(ir_graded), @intFromBool(forced), if (main) "main" else "feed/other",
    });
}

// ---------------------------------------------------------------------------
// glDrawElements thunk
// ---------------------------------------------------------------------------

// The game calls `glDrawElements` through the IAT slot `0x140A935F8`, which is a
// plain pointer variable. The thunk needs the caller's return address to
// recognize the tonemap quad draw, which is not an argument: this stub reads it
// off the stack and passes it as a fifth argument to the body.
//
// Windows x64: at entry `[rsp]` is the return address; the fifth argument goes
// in the caller-allocated shadow-space slot `[rsp+0x20]` (which is
// `[callee_rsp+0x28]`). The 0x28 allocation keeps the stack 16-byte aligned
// before the call.
comptime {
    @export(&glDrawElementsBody, .{ .name = "swi_gl_draw_elements_body", .visibility = .hidden });
    asm (
        \\.text
        \\.globl swi_gl_draw_elements_entry
        \\swi_gl_draw_elements_entry:
        \\    subq $0x28, %rsp
        \\    movq 0x28(%rsp), %rax
        \\    movq %rax, 0x20(%rsp)
        \\    call swi_gl_draw_elements_body
        \\    addq $0x28, %rsp
        \\    ret
    );
}

extern fn swi_gl_draw_elements_entry() callconv(.c) void;

fn glDrawElementsBody(mode: u32, count: i32, kind: u32, indices: ?*const anyopaque, return_address: usize) callconv(API) void {
    fault.enter(stage.FLIR_GL_DRAW_ELEMENTS);
    defer fault.leave();
    // Measures the plugin's own work only: stopped before the game's draw is
    // forwarded (it used to include the driver's draw cost).
    const t0 = if (verbose) state.nowUs() else 0;
    const original = state.ORIG_GL_DRAW_ELEMENTS.load(.seq_cst);
    if (original == 0) return;
    const f: *const fn (u32, i32, u32, ?*const anyopaque) callconv(API) void = @ptrFromInt(original);

    const base = GL_BASE.load(.monotonic);
    const in_tonemap_quad = base != 0 and
        return_address >= base + offsets.RVA_TONEMAP_QUAD and
        return_address < base + offsets.RVA_TONEMAP_QUAD_END;
    if (in_tonemap_quad and !IN_SUBSTITUTE.load(.acquire)) {
        // `0x68BB00` is a shared fullscreen-quad helper, so several passes draw
        // through it: the tonemap uniforms pass (0x68B8C0) marks the quad that
        // carries the graded image and carries this view's IR decision, and only
        // that one learns a pairing or is replaced. The pair/G-buffer/viewport are
        // all read here, in GL order, so submit order cannot point them at another
        // view. Without the uniforms hook every matched quad is treated as a
        // tonemap quad, but no view is ever IR-graded.
        const pending = TONEMAP_NEXT.swap(0, .acq_rel);
        const is_tonemap = pending != 0 or !TONEMAP_HOOK_OK.load(.monotonic);
        if (is_tonemap) {
            // A failure leaves the game's own draw to run, after the state guard
            // has put everything back.
            const camera = TONEMAP_CAMERA.load(.monotonic);
            if (tonemapQuad(pending == 2, camera, .{ .mode = mode, .count = count, .kind = kind, .indices = indices })) return;
        }
    }

    snapshotLightTextures();
    const substituted = !IN_SUBSTITUTE.load(.acquire) and irViewActive() and flirBlockDrawTraced(mode, count, kind, indices);
    if (substituted) {
        if (verbose) _ = HOOK_SUB_US.fetchAdd(state.nowUs() -| t0, .monotonic);
        if (verbose) _ = HOOK_US.fetchAdd(state.nowUs() -| t0, .monotonic);
        return;
    }
    if (verbose) _ = HOOK_US.fetchAdd(state.nowUs() -| t0, .monotonic);
    f(mode, count, kind, indices);
    if (RESTORE_LF.load(.monotonic) >= 0) restoreSkinnedDraw();
}

/// GL `glBindFramebuffer` thunk: keeps the GL-order view state. `CURRENT_FBO` is
/// the draw target a draw is attributed to, `LAST_GBUF` the newest G-buffer-like
/// target bound since the last tonemap quad (the pairing is learned at that
/// quad). Forwards unchanged.
fn glBindFramebufferHook(target: u32, framebuffer: u32) callconv(API) void {
    fault.enter(stage.FLIR_GL_BIND_FRAMEBUFFER);
    defer fault.leave();
    const original = state.ORIG_GL_BIND_FRAMEBUFFER.load(.seq_cst);
    if (original == 0) return;
    const f: *const fn (u32, u32) callconv(API) void = @ptrFromInt(original);
    f(target, framebuffer);
    if (target != GL_FRAMEBUFFER and target != GL_DRAW_FRAMEBUFFER) return;
    CURRENT_FBO.store(framebuffer, .monotonic);
    updateActiveView(framebuffer);
    // The first bind after a tonemap quad identifies that view: FBO 0 is the main
    // view (FXAA presents into the default framebuffer), anything else is a feed
    // or other offscreen view. Consuming it here keeps the association in GL
    // command order.
    const pending = PENDING_QUAD_TARGET.swap(0, .acq_rel);
    if (pending != 0) noteFirstBind(pending, framebuffer);
    if (framebuffer == 0) return;
    // Known targets come from the registry (no GL queries); an unknown one is
    // probed once, while it is bound, and only kept when it has the two colour
    // attachments a G-buffer has.
    if (gl()) |g| {
        if (gbufferFor(g, framebuffer) != null) LAST_GBUF.store(framebuffer, .monotonic);
    }
}

/// GL `glUniformMatrix4fv` thunk: captures the last `mat_view_proj`/`mat_world`
/// upload for the current program (uniform names are queryable on any program
/// object), so the block pass needs no guessed location.
fn glUniformMatrix4fvHook(location: i32, count: i32, transpose: u8, value: ?[*]const f32) callconv(API) void {
    fault.enter(stage.FLIR_GL_UNIFORM_MATRIX);
    defer fault.leave();
    const original = state.ORIG_GL_UNIFORM_MATRIX4FV.load(.seq_cst);
    if (original == 0) return;
    if (vpRecording() and count == 1 and location >= 0) {
        // No VirtualQuery on `value`: the driver reads the same 64 bytes in
        // the forwarded call right below, so a bad pointer faults either way.
        if (value) |v| recordMatrixUpload(location, transpose, v);
    }
    const f: *const fn (i32, i32, u8, ?[*]const f32) callconv(API) void = @ptrFromInt(original);
    f(location, count, transpose, value);
}

/// Only record the matrices while the FLIR block pass could actually replay them:
/// an IR-graded target exists (or a debug F10 view is on) and the mesh->body map
/// is not empty. A matrix upload costs a per-draw `GL_CURRENT_PROGRAM` query, so
/// this stays off while no substitution can ever happen.
fn vpRecording() bool {
    return !MESH_BODY_EMPTY.load(.monotonic) and
        (IR_WANTED.load(.monotonic) or (DEBUG_ASSERTIONS and F10_ON.load(.monotonic)));
}

/// Deferred light program id, its graphics_offset location, the last
/// inverse view-projection it was given (column-major) and graphics_offset.y.
var LIGHT_PROGRAM: Atomic(u32) = .init(0);
var LIGHT_OFFSET_LOC: Atomic(i32) = .init(-1);
var LIGHT_INV: Mutex(?[16]f32) = .{ .value = null };
/// texture_water_depth: the ocean surface's log depth (depth_ocean pass), the
/// game's own "is this pixel under water" input. Snapshotted at the light draw.
var LIGHT_WATER_LOC: Atomic(i32) = .init(-1);
var LIGHT_WATER_UNIT: Atomic(i32) = .init(-1);
var LIGHT_WATER_TEX: Atomic(u32) = .init(0);

fn trackedProgramIs(program: u32) bool {
    return @as(u32, @truncate(@as(u64, @bitCast(TRACK_PROGRAM.load(.monotonic))))) == program;
}

/// At the light pass's own draw: which texture is its water depth.
fn snapshotLightTextures() void {
    const light = LIGHT_PROGRAM.load(.monotonic);
    if (light == 0 or !trackedProgramIs(light)) return;
    var unit = LIGHT_WATER_UNIT.load(.monotonic);
    if (unit < 0) {
        const g = gl() orelse return;
        const loc = LIGHT_WATER_LOC.load(.monotonic);
        if (loc < 0) return;
        // Once per light program: the sampler's unit (5 in the RE notes).
        g.get_uniformiv(light, loc, @ptrCast(&unit));
        LIGHT_WATER_UNIT.store(unit, .monotonic);
        logDebug("FLIR: light pass texture_water_depth on unit {d}", .{unit});
    }
    if (unit >= 0 and unit < 16) LIGHT_WATER_TEX.store(TEX_UNITS[@intCast(unit)].load(.monotonic), .monotonic);
}
var LIGHT_OFFSET_Y_BITS: Atomic(u32) = .init(0);
/// The light pass's own sun_light_direction (points from the sun into the
/// scene): what the game lights and shadows with this frame.
var LIGHT_SUN_LOC: Atomic(i32) = .init(-1);
var LIGHT_SUN: Mutex(?[3]f32) = .{ .value = null };
var ORIG_GL_UNIFORM3F: Atomic(usize) = .init(0);

/// `glUniform3f` thunk: records the light pass's graphics_offset (render
/// origin in world space; the sea is world y = 0).
fn glUniform3fHook(location: i32, x: f32, y: f32, z: f32) callconv(API) void {
    const original = ORIG_GL_UNIFORM3F.load(.monotonic);
    if (original != 0) {
        const f: *const fn (i32, f32, f32, f32) callconv(API) void = @ptrFromInt(original);
        f(location, x, y, z);
    }
    if (location < 0 or !trackedProgramIs(LIGHT_PROGRAM.load(.monotonic))) return;
    if (location == LIGHT_OFFSET_LOC.load(.monotonic)) {
        LIGHT_OFFSET_Y_BITS.store(@bitCast(y), .monotonic);
    } else if (location == LIGHT_SUN_LOC.load(.monotonic)) {
        const n = @sqrt(x * x + y * y + z * z);
        if (std.math.isFinite(n) and n > 1e-6) {
            const s = LIGHT_SUN.lock();
            defer LIGHT_SUN.unlock();
            s.* = .{ x / n, y / n, z / n };
        }
    }
}

fn readMat16(value: [*]const f32) [16]f32 {
    var raw: [16]f32 = undefined;
    @memcpy(&raw, value[0..16]);
    return raw;
}

fn recordMatrixUpload(location: i32, transpose: u8, value: [*]const f32) void {
    const g = gl() orelse return;
    const program_raw = tracked(g, &TRACK_PROGRAM, GL_CURRENT_PROGRAM);
    if (program_raw <= 0) return;
    const program: u32 = @intCast(program_raw);
    const known = blk: {
        const mats = PROG_MATS.lock();
        defer PROG_MATS.unlock();
        break :blk mats.contains(program);
    };
    if (!known) {
        // First upload seen for this program: two location queries, once.
        glq(@src());
        const vp_loc = g.get_uniform_location(program, "mat_view_proj");
        glq(@src());
        const world_loc = g.get_uniform_location(program, "mat_world");
        // The deferred light pass (light.glslf) is the only program with both
        // texture_water_depth and graphics_offset: its inverse view-projection
        // and render origin let the composite cast rays (sea plane, sun disc).
        glq(@src());
        const water = g.get_uniform_location(program, "texture_water_depth");
        glq(@src());
        const offset = g.get_uniform_location(program, "graphics_offset");
        var light_inv: i32 = -1;
        if (water >= 0 and offset >= 0) {
            LIGHT_PROGRAM.store(program, .monotonic);
            LIGHT_OFFSET_LOC.store(offset, .monotonic);
            LIGHT_WATER_LOC.store(water, .monotonic);
            LIGHT_WATER_UNIT.store(-1, .monotonic);
            glq(@src());
            LIGHT_SUN_LOC.store(g.get_uniform_location(program, "sun_light_direction"), .monotonic);
            glq(@src());
            light_inv = g.get_uniform_location(program, "mat_view_proj_inverse");
        }
        const mats = PROG_MATS.lock();
        defer PROG_MATS.unlock();
        if (mats.count() >= 1024) mats.clearRetainingCapacity();
        mats.put(gpa, program, .{ .locs = .{ vp_loc, world_loc }, .light_inv = light_inv }) catch return;
    }
    var is_light_inv = false;
    {
        const mats = PROG_MATS.lock();
        defer PROG_MATS.unlock();
        const e = mats.getPtr(program) orelse return;
        if (location >= 0 and location == e.light_inv) {
            is_light_inv = true;
        } else {
            if (location != e.locs[0] and location != e.locs[1]) return;
            const m: Mat = .{ .m = readMat16(value), .transpose = transpose };
            if (location == e.locs[0]) e.vp = m else e.world = m;
        }
    }
    if (is_light_inv) {
        // Same view as the tonemap quad that follows (light pass runs per view
        // right before it).
        const inv = LIGHT_INV.lock();
        defer LIGHT_INV.unlock();
        inv.* = toColmajor(&readMat16(value), transpose);
        return;
    }
    logOnce(LOG_FLIR_VP, "FLIR: mat_view_proj/mat_world captured from glUniformMatrix4fv by name", .{});
}

// ---------------------------------------------------------------------------
// View selection
// ---------------------------------------------------------------------------

/// True when the draw that is running belongs to a view we replace. A G-buffer is
/// identified by the pairing learned at its view's previous tonemap quad; a
/// G-buffer that is not paired yet (or whose target is not IR-forced) is never
/// substituted, because its target - and therefore its view - is unknown.
fn irViewActive() bool {
    // An empty mesh->body map means no draw can be substituted, so the whole
    // block pass (per-draw GL queries, map lock) stays out of the game's draws.
    if (MESH_BODY_EMPTY.load(.monotonic) or state.BLOCKPASS_DISABLED.load(.monotonic) or
        BLOCK_FAILED.load(.monotonic) or COMPOSITE_FAILED.load(.monotonic))
    {
        return false;
    }
    return CURRENT_FBO.load(.monotonic) != 0 and ACTIVE_VIEW.load(.monotonic) != 0;
}

var GET_KEY_STATE: Atomic(usize) = .init(0);
var GET_KEY_STATE_RESOLVED: Atomic(bool) = .init(false);
var LAST_POLL_US: Atomic(u64) = .init(0);

/// F10 edge-detect (debug builds only, per the design contract).
fn f10Toggled() bool {
    if (!DEBUG_ASSERTIONS) return false;
    if (!GET_KEY_STATE_RESOLVED.load(.acquire)) {
        const module = win.LoadLibraryA("user32.dll");
        const address: usize = if (module != null) (if (win.GetProcAddress(module, "GetAsyncKeyState")) |p| @intFromPtr(p) else 0) else 0;
        GET_KEY_STATE.store(address, .monotonic);
        GET_KEY_STATE_RESOLVED.store(true, .release);
    }
    const address = GET_KEY_STATE.load(.monotonic);
    if (address == 0) return F10_ON.load(.monotonic);
    // Poll at most every 8 ms: it is read from the per-draw gate and per quad.
    const now = state.nowUs();
    if (now -| LAST_POLL_US.load(.monotonic) < 8000) return F10_ON.load(.monotonic);
    LAST_POLL_US.store(now, .monotonic);
    const f: *const fn (i32) callconv(API) i16 = @ptrFromInt(address);
    const down = @as(u16, @bitCast(f(0x79))) & 0x8000 != 0; // VK_F10
    if (down and !KEY_HELD.swap(true, .monotonic)) {
        const now_on = !F10_ON.load(.monotonic);
        F10_ON.store(now_on, .monotonic);
        // The forced-G-buffer mirror follows the toggle immediately, so the next
        // draws of the main view already use it.
        refreshForcedGbuffers();
        // Once per distinct state: an F10 that never reaches the view has to be
        // distinguishable from a composite that is drawn but invisible.
        logOnceKey((@as(u64, 4) << 56) | @intFromBool(now_on), "FLIR: F10 edge detected -> debug view {s} (ir_view_active={})", .{ if (now_on) "on" else "off", irViewActive() });
    } else if (!down) {
        KEY_HELD.store(false, .monotonic);
    }
    return F10_ON.load(.monotonic);
}

// ---------------------------------------------------------------------------
// Mesh -> body map
// ---------------------------------------------------------------------------

/// Element budget of one mesh-map walk. The walk runs every 250 ms on the GAME
/// thread (see `sceneTick`) and each element costs a `readable` (VirtualQuery)
/// call, so a garbage layer count must not turn into a multi-million-call
/// stall.
const MAX_MESH_ELEMENTS: u32 = 1 << 16;
/// Cap on one body's chunk-item set, and on its modulus (a set header's slot
/// count), matching the sim's walker caps.
const MAX_CHUNK_ITEMS: u32 = 8192;

/// Walk `scene -> vehicles -> bodies -> body+0x540 chunk container` and record
/// `chunk mesh index buffer -> body`. GAME THREAD ONLY: the game rebuilds these
/// containers while a vehicle spawns, so walking them from the render thread
/// races the mutation (it crashed the game). The render thread only reads the
/// published map.
///
/// Each inner element holds a group pointer in its first field and the mesh is
/// `*(*(element+0))+8` (verified by disassembly of the 232-byte item walk); the
/// mesh's IBO id is `mesh+0x64` (u32). Chunk geometry is executed by
/// `sub_14067DE50`, not by `0x68dd10`, so the IBO id is what the draw thunk can
/// see. A miss only means the draw is left to the game.
/// element -> (mesh, IBO id): mesh = *(*(element))+8, IBO at mesh+0x64.
fn elementMesh(element: usize) ?struct { mesh: usize, ibo: u32 } {
    if (element < PTR_MIN or !readable(element, 8)) return null;
    const group = readUsizeAt(element);
    if (group < PTR_MIN or !readable(group + 0x08, 8)) return null;
    const mesh = readUsizeAt(group + 0x08);
    if (mesh < PTR_MIN or !readable(mesh + offsets.MESH_IBO, 4)) return null;
    const ibo = readU32At(mesh + offsets.MESH_IBO);
    return if (ibo != 0) .{ .mesh = mesh, .ibo = ibo } else null;
}

/// Element addresses of a game ringvec `{ptr, u32 cap, u32 start, u32 count}`,
/// appended to `out`. Nothing when the header is implausible or the storage is
/// unreadable.
fn ringvecElems(base: usize, stride: usize, out: *std.ArrayList(usize)) void {
    if (base < PTR_MIN or !readable(base, 0x14)) return;
    const ptr = readUsizeAt(base);
    const cap = readU32At(base + 8);
    const start = readU32At(base + 0xC);
    const count = readU32At(base + 0x10);
    if (ptr < PTR_MIN or cap == 0 or cap > MAX_CHUNK_ITEMS or count > cap or !readable(ptr, @as(usize, cap) * stride)) return;
    for (0..count) |i| out.append(gpa, ptr + stride * ((i + start) % cap)) catch return;
}

const MeshOwner = struct { mesh: usize, body: usize };

const MeshWalk = struct {
    map: *fx.HashMap(u32, usize),
    owner: *fx.HashMap(u32, MeshOwner),
    coll_mesh: u32 = 0,
    coll_body: u32 = 0,
    bodies: usize = 0,
    meshes: usize = 0,
    walked: u32 = 0,

    fn visit(self: *MeshWalk, _: u32, body: usize) bool {
        if (self.walked >= MAX_MESH_ELEMENTS) return false;
        // re_chunks: body+0x540 and body+0x588 are ringvecs of 88-byte chunk
        // entries; each entry starts with an inline ringvec of 96-byte
        // instances holding three geometry pointers (+0/+0x10/+0x20); the GL
        // holder is *(res+8) with the IBO at +0x64.
        self.bodies += 1;
        var body_ibos: u32 = 0;
        var entries: std.ArrayList(usize) = .empty;
        defer entries.deinit(gpa);
        var insts: std.ArrayList(usize) = .empty;
        defer insts.deinit(gpa);
        var elements: std.ArrayList(usize) = .empty;
        defer elements.deinit(gpa);
        for ([_]usize{ offsets.BODY_CHUNK_ITEMS, offsets.BODY_CHUNK_ITEMS_B }) |table| {
            entries.clearRetainingCapacity();
            ringvecElems(body + table, 88, &entries);
            for (entries.items) |entry| {
                insts.clearRetainingCapacity();
                ringvecElems(entry, 96, &insts);
                for (insts.items) |inst| {
                    for ([_]usize{ 0, 0x10, 0x20 }) |slot| {
                        if (readable(inst + slot, 8)) elements.append(gpa, inst + slot) catch {};
                    }
                }
            }
        }
        for (elements.items) |element| {
            if (self.walked >= MAX_MESH_ELEMENTS) return false;
            self.walked += 1;
            const found = elementMesh(element) orelse continue;
            if (self.owner.get(found.ibo)) |prev| {
                if (prev.mesh != found.mesh) self.coll_mesh += 1;
                if (prev.body != body) self.coll_body += 1;
                if ((prev.mesh != found.mesh or prev.body != body) and self.coll_mesh + self.coll_body <= 8) {
                    logDebug("FLIR: IBO collision ibo={d}: mesh 0x{x}/body 0x{x} vs mesh 0x{x}/body 0x{x} (later wins)", .{ found.ibo, prev.mesh, prev.body, found.mesh, body });
                }
            } else {
                body_ibos += 1;
            }
            self.owner.put(gpa, found.ibo, .{ .mesh = found.mesh, .body = body }) catch {};
            self.map.put(gpa, found.ibo, body) catch {};
            self.meshes += 1;
        }
        if (body_ibos > 0) logOnceKey(body, "FLIR: body 0x{x} mapped {d} mesh IBOs", .{ body, body_ibos });
        return true;
    }
};

fn rebuildMeshBody(scene: usize) void {
    var map: fx.HashMap(u32, usize) = .empty;
    var owner: fx.HashMap(u32, MeshOwner) = .empty;
    defer owner.deinit(gpa);
    var walk: MeshWalk = .{ .map = &map, .owner = &owner };
    _ = blocktemp.forEachBody(scene, &walk, MeshWalk.visit);
    const count = map.count();
    if (walk.coll_mesh + walk.coll_body > 0) {
        logDebug("FLIR: IBO collisions: same-ibo-different-mesh={d} same-ibo-different-body={d} (meshes walked {d}, unique ibos {d})", .{ walk.coll_mesh, walk.coll_body, walk.meshes, count });
    }
    {
        const published = MESH_BODY.lock();
        defer MESH_BODY.unlock();
        published.deinit(gpa);
        published.* = map;
    }
    MESH_BODY_EMPTY.store(count == 0, .monotonic);
    if (count > 0) {
        logOnce(LOG_FLIR_MESH_MAP, "FLIR: mesh->body map from scene 0x{x} (game thread, keyed by IBO id): bodies={d} meshes={d} (elements walked {d})", .{ scene, walk.bodies, count, walk.walked });
    } else {
        logOnce(LOG_FLIR_MESH_MAP, "FLIR: mesh->body map EMPTY (bodies={d}); body+0x{x} / container layout differs, block pass disabled", .{ walk.bodies, offsets.BODY_CHUNK_ITEMS });
    }
}

// ---------------------------------------------------------------------------
// Block pass
// ---------------------------------------------------------------------------

var LAST_CLASSIFIED: Atomic(u32) = .init(0);

/// Re-issue a vehicle chunk draw with the FLIR block program. Returns false when
/// the draw must be left to the game (nothing is changed in that case).
fn flirBlockDraw(mode: u32, count: i32, kind: u32, indices: ?*const anyopaque, t: *Trace) bool {
    t.reason = SK_BAD_DRAW;
    // Only the mesh's own indexed triangle draw qualifies. `indices` is NOT
    // checked: it is a byte offset into the bound IBO (`first * 2`), so a valid
    // first == 0 draw passes NULL, and we never dereference it.
    const block_shape = mode == GL_TRIANGLES and kind == GL_UNSIGNED_SHORT and count > 0;
    t.reason = SK_VIEW;
    const g = gl() orelse return false;
    // G-buffer pass only: never draw our normal/temperature program into a
    // depth/shadow target, and never into a G-buffer whose target is not
    // IR-forced. The bound draw framebuffer is the one tracked in GL order (no
    // driver query), and the pairing was learned from a previous frame's tonemap
    // quad - an unpaired G-buffer is never substituted in.
    const fbo = CURRENT_FBO.load(.monotonic);
    if (fbo == 0 or !irViewActive()) return false;
    {
        // Classify each program once (cached), so skinned draws are known even
        // before any vehicle chunk has been drawn in this view.
        const program = tracked(g, &TRACK_PROGRAM, GL_CURRENT_PROGRAM);
        if (program > 0 and LAST_CLASSIFIED.swap(@intCast(program), .monotonic) != @as(u32, @intCast(program))) {
            _ = opaqueProgram(g, @intCast(program));
        }
        if (program > 0 and count > 0) {
            if (particleSlot(@intCast(program))) |ps| {
                const gb = cachedGbuffer(fbo) orelse return false;
                if (gb.normal_encoded) return false;
                return particleDraw(g, ps.slot, ps.prog, mode, count, kind, indices);
            }
        }
        if (program > 0 and SKINNED[0].program.load(.monotonic) != 0 and skinnedSlot(@intCast(program)) != null) {
            if (cachedGbuffer(fbo) != null) markSkinnedDraw(g, @intCast(program));
            return false;
        }
    }
    t.reason = SK_BAD_DRAW;
    // Block substitution: only the mesh's own indexed triangle draw qualifies.
    if (!block_shape) return false;
    // The game binds the mesh's IBO before the draw; that id is the map key. It
    // is checked before any framebuffer lookup, so a pass full of non-vehicle
    // draws costs only one GL query each.
    t.reason = SK_NO_IBO;
    const ibo = tracked(g, &TRACK_IBO, GL_ELEMENT_ARRAY_BUFFER_BINDING);
    t.ibo = ibo;
    if (ibo <= 0) return false;
    const mapped: ?usize = blk: {
        const map = MESH_BODY.lock();
        if (map.get(@intCast(ibo))) |body| {
            MESH_BODY.unlock();
            break :blk body;
        }
        const entries = map.count();
        MESH_BODY.unlock();
        diagCount(DIAG_UNMAPPED);
        // Not a chunk IBO: possibly an individually-meshed component
        // (resolved by position below); otherwise the game draw is kept.
        logOnce(LOG_FLIR_MESH_MISS, "FLIR: draw IBO {d} is not in the mesh->body map ({d} entries); tried as a component draw, else the game draw is kept (body+0x{x} layout may differ)", .{ ibo, entries, offsets.BODY_CHUNK_ITEMS });
        // Cheap rejection: no chunk drawn yet -> no body to match.
        if (!hasBodyMats()) {
            t.reason = SK_NO_MATS;
            return false;
        }
        break :blk null;
    };
    t.mapped = mapped != null;
    t.reason = SK_NO_GBUF;
    const gbuffer = cachedGbuffer(fbo) orelse return false;
    const program = tracked(g, &TRACK_PROGRAM, GL_CURRENT_PROGRAM);
    t.program = program;
    t.reason = SK_PROGRAM;
    if (program <= 0 or !opaqueProgram(g, @intCast(program))) return false;
    // Read both matrices from the game's program itself: a uniform persists per
    // program, so the last `glUniformMatrix4fv` seen by the hook (any program)
    // is not necessarily the value this draw uses when the game skips redundant
    // uploads and interleaves programs (draw-order, hence angle, dependent).
    const cached = blk: {
        const mats = PROG_MATS.lock();
        defer PROG_MATS.unlock();
        break :blk mats.get(@intCast(program));
    };
    const pm = cached orelse {
        t.reason = SK_NO_MATRIX;
        return false;
    };
    // Never replay another program's matrices.
    if (pm.vp == null or pm.world == null) {
        t.reason = SK_NO_MATRIX;
        return false;
    }
    {
        const vp = VIEW_PROJ.lock();
        defer VIEW_PROJ.unlock();
        vp.* = pm.vp.?.m;
    }
    VIEW_PROJ_TRANSPOSE.store(pm.vp.?.transpose, .monotonic);
    VIEW_PROJ_VALID.store(true, .monotonic);
    {
        const w = WORLD_MATRIX.lock();
        defer WORLD_MATRIX.unlock();
        w.* = pm.world.?.m;
    }
    WORLD_TRANSPOSE.store(pm.world.?.transpose, .monotonic);
    WORLD_VALID.store(true, .monotonic);
    t.reason = SK_NO_MATRIX;
    if (!VIEW_PROJ_VALID.load(.monotonic) or !WORLD_VALID.load(.monotonic)) {
        logOnce(LOG_FLIR_VP_MISS, "FLIR: mat_view_proj/mat_world not captured yet; block pass skipped", .{});
        return false;
    }
    const world_cm = toColmajor(&pm.world.?.m, pm.world.?.transpose);
    t.reason = SK_NO_COMPONENT;
    var body: usize = undefined;
    var comp_to_body: CompToBody = undefined;
    if (mapped) |b| {
        recordBodyMat(b, world_cm);
        body = b;
        comp_to_body = IDENTITY;
    } else {
        const found = resolveComponent(g, @intCast(program), @intCast(ibo), count, indices, &world_cm) orelse return false;
        body = found.body;
        comp_to_body = found.to_body;
    }
    t.body = body;
    const component = mapped == null;
    t.reason = SK_BEGIN;
    // Breadcrumb for anything in the substitute (texture upload, our GL calls,
    // the game's driver) that can still fault.
    fault.set(stage.FLIR_BLOCK_DRAW);
    IN_SUBSTITUTE.store(true, .release);
    const saved = beginBlockDraw(g, body, @intCast(ibo), count, indices, gbuffer.normal_encoded, &comp_to_body) orelse {
        IN_SUBSTITUTE.store(false, .release);
        return false;
    };
    g.draw_elements(mode, count, kind, indices);
    endBlockDraw(g, saved);
    IN_SUBSTITUTE.store(false, .release);
    diagCount(if (component) DIAG_COMPONENT else DIAG_SUBSTITUTED);
    t.reason = SK_OK;
    return true;
}

// ---------------------------------------------------------------------------
// Per-reason skip counters (verbose builds) and the F8 one-frame draw dump
// ---------------------------------------------------------------------------

const SK_OK: u8 = 0;
const SK_BAD_DRAW: u8 = 1;
const SK_NO_IBO: u8 = 2;
const SK_NO_MATS: u8 = 3;
const SK_NO_GBUF: u8 = 4;
const SK_PROGRAM: u8 = 5;
const SK_NO_MATRIX: u8 = 6;
const SK_NO_COMPONENT: u8 = 7;
const SK_BEGIN: u8 = 8;
const SK_VIEW: u8 = 9;
const SK_N: usize = 10;
const SK_NAMES = [SK_N][]const u8{
    "substituted",     "not_tri_u16", "no_ibo",               "unmapped_no_bodymats", "no_gbuffer",
    "not_opaque_prog", "no_matrix",   "component_unresolved", "begin_failed",         "view_inactive",
};

/// What one draw hit in `flirBlockDraw`.
const Trace = struct {
    reason: u8,
    ibo: i32,
    program: i32,
    mapped: bool,
    count: i32,
    body: usize,
};

var SKIP_COUNTS: [2][SK_N]Atomic(u32) = [_][SK_N]Atomic(u32){[_]Atomic(u32){.init(0)} ** SK_N} ** 2;

/// 0 = idle, 1 = armed (waits for a main-view quad), 2 = recording the next
/// main-view frame.
var DUMP_STATE: Atomic(u32) = .init(0);
var F8_HELD: Atomic(bool) = .init(false);
const DumpKey = struct { ibo: i32, reason: u8, fbo: u32, program: i32, mapped: bool, view: u32, body: usize };
const DumpRow = struct { key: DumpKey, draws: u32, idx: u64, max_idx: i32 };
var DUMP: Mutex(std.ArrayList(DumpRow)) = .{ .value = .empty };

fn flirBlockDrawTraced(mode: u32, count: i32, kind: u32, indices: ?*const anyopaque) bool {
    var t: Trace = .{ .reason = SK_VIEW, .ibo = 0, .program = 0, .mapped = false, .count = count, .body = 0 };
    const done = flirBlockDraw(mode, count, kind, indices, &t);
    if (ACTIVE_VIEW.load(.monotonic) == 1) {
        if (verbose) _ = SKIP_COUNTS[@intFromBool(t.mapped)][t.reason].fetchAdd(1, .monotonic);
        if (DUMP_STATE.load(.monotonic) == 2) {
            const key: DumpKey = .{ .ibo = t.ibo, .reason = t.reason, .fbo = CURRENT_FBO.load(.monotonic), .program = t.program, .mapped = t.mapped, .view = ACTIVE_VIEW.load(.monotonic), .body = t.body };
            const dump = DUMP.lock();
            defer DUMP.unlock();
            const n: u64 = @intCast(@max(t.count, 0));
            for (dump.items) |*row| {
                if (std.meta.eql(row.key, key)) {
                    row.draws += 1;
                    row.idx += n;
                    row.max_idx = @max(row.max_idx, t.count);
                    break;
                }
            } else if (dump.items.len < 4096) {
                dump.append(gpa, .{ .key = key, .draws = 1, .idx = n, .max_idx = t.count }) catch {};
            }
        }
    }
    return done;
}

/// Called at every main-view tonemap quad (a frame boundary of the main view):
/// F8 edge arms the dump, the next boundary starts recording, the one after
/// prints it.
fn dumpStep() void {
    if (!DEBUG_ASSERTIONS) return;
    if (asyncKeyDown(0x77)) |down| {
        if (down and !F8_HELD.swap(true, .monotonic)) {
            if (DUMP_STATE.load(.monotonic) == 0) {
                DUMP_STATE.store(1, .monotonic);
                logLine("FLIR: F8 draw dump armed (needs F10 on)");
            }
        } else if (!down) {
            F8_HELD.store(false, .monotonic);
        }
    }
    switch (DUMP_STATE.load(.monotonic)) {
        1 => {
            const dump = DUMP.lock();
            defer DUMP.unlock();
            dump.clearRetainingCapacity();
            DUMP_STATE.store(2, .monotonic);
        },
        2 => {
            DUMP_STATE.store(0, .monotonic);
            var rows: std.ArrayList(DumpRow) = blk: {
                const dump = DUMP.lock();
                defer DUMP.unlock();
                const taken = dump.*;
                dump.* = .empty;
                break :blk taken;
            };
            defer rows.deinit(gpa);
            std.mem.sort(DumpRow, rows.items, {}, struct {
                fn lessThan(_: void, a: DumpRow, b: DumpRow) bool {
                    return a.draws > b.draws;
                }
            }.lessThan);
            var total: u32 = 0;
            for (rows.items) |r| total +%= r.draws;
            const map_n = blk: {
                const m = MESH_BODY.lock();
                defer MESH_BODY.unlock();
                break :blk m.count();
            };
            const mats = blk: {
                const m = BODY_MATS.lock();
                defer BODY_MATS.unlock();
                break :blk m.items.len;
            };
            var out: std.ArrayList(u8) = .empty;
            defer out.deinit(gpa);
            out.print(gpa, "FLIR: F8 main-view draw dump: {d} draws, {d} groups, map={d} body_mats={d} frame={d} (top 58 by count; ibo reason fbo program map/comp view body draws idx max_idx)", .{
                total, rows.items.len, map_n, mats, FRAME.load(.monotonic),
            }) catch return;
            for (rows.items[0..@min(rows.items.len, 58)]) |r| {
                out.print(gpa, "\n  ibo={d} {s} fbo={d} prog={d} {s} view={d} body=0x{x} n={d} idx={d} max={d}", .{
                    r.key.ibo, SK_NAMES[r.key.reason], r.key.fbo, r.key.program, if (r.key.mapped) "map" else "comp", r.key.view, r.key.body, r.draws, r.idx, r.max_idx,
                }) catch break;
            }
            logLine(out.items);
        },
        else => {},
    }
}

// ---------------------------------------------------------------------------
// Component draws (individually-meshed parts): body identified by position
// ---------------------------------------------------------------------------

const IDENTITY = [16]f32{ 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1 };
const MAX_BODY_MATS: usize = 64;

/// The component -> body transform a substituted draw uses (identity for a
/// chunk draw).
const CompToBody = [16]f32;

/// A substituted body's chunk `mat_world` (column-major, camera-origin-relative).
const BodyMat = struct {
    frame: u64,
    view: u32,
    body: usize,
    mat: [16]f32,
    /// Squared distance (m^2) from the body origin past which nothing can be
    /// in its grid; INFINITY until the body has a grid. Refreshed each frame.
    reach2: f32,
};

/// Squared reach of a grid from its body origin (farthest corner + 1 voxel).
fn gridReach2(min: [3]i32, dims: [3]u32) f32 {
    var sum: f32 = 0;
    for (0..3) |i| {
        const lo: i64 = @as(i64, min[i]) - 1;
        const hi: i64 = @as(i64, min[i]) + dims[i] + 1;
        const m = util.f32_(@max(@abs(lo), @abs(hi))) * 0.25;
        sum += m * m;
    }
    return sum;
}

fn bodyReach2(body: usize) f32 {
    const textures = GRID_TEXTURES.lock();
    defer GRID_TEXTURES.unlock();
    const g = textures.get(body) orelse return std.math.inf(f32);
    return gridReach2(g.min, g.dims);
}

/// Newest chunk matrix per (view, body); entries older than the previous frame
/// are dropped, so a lookup sees this frame's matrices (fallback: last frame's).
var BODY_MATS: Mutex(std.ArrayList(BodyMat)) = .{ .value = .empty };

var COMP_LOGGED: Mutex(std.ArrayList(u32)) = .{ .value = .empty };

/// Raw upload (with the game's transpose flag) -> column-major matrix.
fn toColmajor(raw: *const [16]f32, transpose: u8) [16]f32 {
    if (transpose == 0) return raw.*;
    var m: [16]f32 = undefined;
    for (0..4) |c| {
        for (0..4) |r| m[c * 4 + r] = raw[r * 4 + c];
    }
    return m;
}

fn matMul(a: *const [16]f32, b: *const [16]f32) [16]f32 {
    var m: [16]f32 = undefined;
    for (0..4) |c| {
        for (0..4) |r| {
            var sum: f32 = 0;
            for (0..4) |k| sum += a[k * 4 + r] * b[c * 4 + k];
            m[c * 4 + r] = sum;
        }
    }
    return m;
}

fn cross3(u: [3]f32, v: [3]f32) [3]f32 {
    return .{ u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0] };
}

/// Inverse of an affine column-major matrix (bottom row assumed 0,0,0,1).
fn invAffine(m: *const [16]f32) ?[16]f32 {
    const a = [3]f32{ m[0], m[1], m[2] };
    const b = [3]f32{ m[4], m[5], m[6] };
    const c = [3]f32{ m[8], m[9], m[10] };
    const r0 = cross3(b, c);
    const r1 = cross3(c, a);
    const r2 = cross3(a, b);
    const det = a[0] * r0[0] + a[1] * r0[1] + a[2] * r0[2];
    if (!std.math.isFinite(det) or @abs(det) < 1e-12) return null;
    const d = 1.0 / det;
    // Inverse columns are r0/det, r1/det, r2/det as ROWS of the inverse.
    const rows = [3][3]f32{ r0, r1, r2 };
    const t = [3]f32{ m[12], m[13], m[14] };
    var o = [_]f32{0} ** 16;
    for (rows, 0..) |row, i| {
        for (0..3) |j| o[j * 4 + i] = row[j] * d;
        o[12 + i] = -(row[0] * t[0] + row[1] * t[1] + row[2] * t[2]) * d;
    }
    o[15] = 1.0;
    return o;
}

fn hasBodyMats() bool {
    const view = ACTIVE_VIEW.load(.monotonic);
    const mats = BODY_MATS.lock();
    defer BODY_MATS.unlock();
    for (mats.items) |e| if (e.view == view) return true;
    return false;
}

fn recordBodyMat(body: usize, mat: [16]f32) void {
    const frame = FRAME.load(.monotonic);
    const view = ACTIVE_VIEW.load(.monotonic);
    const mats = BODY_MATS.lock();
    defer BODY_MATS.unlock();
    for (mats.items) |*e| {
        if (e.view == view and e.body == body) {
            if (e.frame != frame) e.reach2 = bodyReach2(body);
            e.frame = frame;
            e.mat = mat;
            return;
        }
    }
    var keep: usize = 0;
    for (mats.items) |e| {
        if (e.frame +| 1 >= frame) {
            mats.items[keep] = e;
            keep += 1;
        }
    }
    mats.shrinkRetainingCapacity(keep);
    if (mats.items.len < MAX_BODY_MATS) mats.append(gpa, .{ .frame = frame, .view = view, .body = body, .mat = mat, .reach2 = bodyReach2(body) }) catch {};
}

/// Local-space vertex bbox of one draw's index range (min, max), raw floats.
const DrawBox = struct { lo: [3]f32, hi: [3]f32 };
const MAX_BOX_CACHE: usize = 4096;
const DrawKey = struct { ibo: u32, first: usize, count: i32 };
/// (ibo, first, count) -> bbox (null = unreadable; never retried).
var DRAW_BOXES: Mutex(fx.HashMap(DrawKey, ?DrawBox)) = .{ .value = .empty };
const ChoiceKey = struct { ibo: u32, first: usize, count: i32, set: u64 };
/// (ibo, first, count, candidate-set hash) -> chosen body (matrix-matched draws).
var BODY_CHOICE: Mutex(fx.HashMap(ChoiceKey, usize)) = .{ .value = .empty };
var CAND_LOGGED: Mutex(std.ArrayList(u32)) = .{ .value = .empty };

/// GPU readbacks allowed per frame (each glGetBufferSubData stalls the pipeline).
const MAX_READBACKS_PER_FRAME: u32 = 2;
var READBACK_FRAME: Atomic(u64) = .init(std.math.maxInt(u64));
var READBACK_USED: Atomic(u32) = .init(0);
var BBOX_LOGGED: Atomic(u32) = .init(0);

fn takeReadbackBudget() bool {
    const frame = FRAME.load(.monotonic);
    if (READBACK_FRAME.swap(frame, .monotonic) != frame) READBACK_USED.store(0, .monotonic);
    return READBACK_USED.fetchAdd(1, .monotonic) < MAX_READBACKS_PER_FRAME;
}

const OptBox = struct {
    v: ?DrawBox,
    pub fn format(self: OptBox, writer: *std.Io.Writer) std.Io.Writer.Error!void {
        if (self.v) |b| try writer.print("Some(({any}, {any}))", .{ b.lo, b.hi }) else try writer.writeAll("None");
    }
};

/// Vertex bbox of the draw, read back from the GPU at most once per key.
/// Outer null = over this frame's readback budget (nothing cached; retry later).
fn drawBbox(g: *const Gl, program: u32, ibo: u32, count: i32, indices: ?*const anyopaque) ??DrawBox {
    const first = @intFromPtr(indices) / 2;
    const key: DrawKey = .{ .ibo = ibo, .first = first, .count = count };
    {
        const boxes = DRAW_BOXES.lock();
        defer DRAW_BOXES.unlock();
        if (boxes.get(key)) |v| return v;
    }
    if (!takeReadbackBudget()) return null;
    const started = state.nowUs();
    const found = readDrawBbox(g, program, count, first);
    {
        const boxes = DRAW_BOXES.lock();
        defer DRAW_BOXES.unlock();
        if (boxes.count() >= MAX_BOX_CACHE) boxes.clearRetainingCapacity();
        boxes.put(gpa, key, found) catch {};
    }
    if (verbose and BBOX_LOGGED.fetchAdd(1, .monotonic) < 64) {
        logf("FLIR: vertex bbox readback ibo={d} first={d} count={d}: {f} in {d} us", .{ ibo, first, count, OptBox{ .v = found }, state.nowUs() -| started });
    }
    return found;
}

const ARRAY_BUFFER: u32 = 0x8892;
const ARRAY_BUFFER_BINDING: u32 = 0x8894;
const ELEMENT_ARRAY_BUFFER: u32 = 0x8893;
const BUFFER_SIZE: u32 = 0x8764;

/// The position attribute of `program` as the game set it up for this draw.
const PosAttrib = struct { loc: u32, vbo: i32, size: i32, kind: i32, stride: i32, norm: i32, offset: usize };

fn positionAttrib(g: *const Gl, program: u32) ?PosAttrib {
    glq(@src());
    const loc = g.get_attrib_location(program, "vertex_position_in");
    if (loc < 0) return null;
    var a: PosAttrib = .{ .loc = @intCast(loc), .vbo = 0, .size = 0, .kind = 0, .stride = 0, .norm = 0, .offset = 0 };
    glq(@src());
    g.get_vertex_attribiv(a.loc, 0x889f, &a.vbo);
    glq(@src());
    g.get_vertex_attribiv(a.loc, 0x8623, &a.size);
    glq(@src());
    g.get_vertex_attribiv(a.loc, 0x8625, &a.kind);
    glq(@src());
    g.get_vertex_attribiv(a.loc, 0x8624, &a.stride);
    glq(@src());
    g.get_vertex_attribiv(a.loc, 0x886a, &a.norm);
    var ptr: ?*const anyopaque = null;
    glq(@src());
    g.get_vertex_attrib_pointerv(a.loc, 0x8645, &ptr);
    a.offset = @intFromPtr(ptr);
    return a;
}

fn readF32Le(data: []const u8, o: usize) f32 {
    return @bitCast(std.mem.readInt(u32, data[o..][0..4], .little));
}

fn readDrawBbox(g: *const Gl, program: u32, count: i32, first: usize) ?DrawBox {
    const a = positionAttrib(g, program) orelse return null;
    if (@as(u32, @bitCast(a.kind)) != GL_FLOAT or a.size < 3 or a.vbo <= 0) return null;
    const n: usize = @min(@as(usize, @intCast(@max(count, 0))), 1 << 18);
    var ebo_size: i32 = 0;
    glq(@src());
    g.get_buffer_parameteriv(ELEMENT_ARRAY_BUFFER, BUFFER_SIZE, &ebo_size);
    if (n == 0 or (first + n) * 2 > @as(usize, @intCast(@max(ebo_size, 0)))) return null;
    const idx = gpa.alloc(u16, n) catch return null;
    defer gpa.free(idx);
    glq(@src());
    g.get_buffer_sub_data(ELEMENT_ARRAY_BUFFER, @intCast(first * 2), @intCast(n * 2), idx.ptr);
    var imin: u16 = std.math.maxInt(u16);
    var imax: u16 = 0;
    for (idx) |i| {
        imin = @min(imin, i);
        imax = @max(imax, i);
    }
    const step: usize = if (a.stride > 0) @intCast(a.stride) else @as(usize, @intCast(a.size)) * 4;
    const span = @as(usize, imax) - imin + 1;
    const start = a.offset + @as(usize, imin) * step;
    const want = span * step;
    var prev_array: i32 = 0;
    glq(@src());
    g.get_integerv(ARRAY_BUFFER_BINDING, @ptrCast(&prev_array));
    g.bind_buffer(ARRAY_BUFFER, @intCast(a.vbo));
    var bsize: i32 = 0;
    glq(@src());
    g.get_buffer_parameteriv(ARRAY_BUFFER, BUFFER_SIZE, &bsize);
    const ok = start + want <= @as(usize, @intCast(@max(bsize, 0)));
    const data = gpa.alloc(u8, if (ok) want else 0) catch {
        g.bind_buffer(ARRAY_BUFFER, @bitCast(prev_array));
        return null;
    };
    defer gpa.free(data);
    if (ok) {
        glq(@src());
        g.get_buffer_sub_data(ARRAY_BUFFER, @intCast(start), @intCast(want), data.ptr);
    }
    g.bind_buffer(ARRAY_BUFFER, @bitCast(prev_array));
    if (!ok) return null;
    var lo = [3]f32{ std.math.floatMax(f32), std.math.floatMax(f32), std.math.floatMax(f32) };
    var hi = [3]f32{ -std.math.floatMax(f32), -std.math.floatMax(f32), -std.math.floatMax(f32) };
    for (idx) |i| {
        const o = (@as(usize, i) - imin) * step;
        if (o + 12 > data.len) return null;
        for (0..3) |axis| {
            const v = readF32Le(data, o + axis * 4);
            if (!std.math.isFinite(v)) return null;
            lo[axis] = @min(lo[axis], v);
            hi[axis] = @max(hi[axis], v);
        }
    }
    return .{ .lo = lo, .hi = hi };
}

/// Voxel bbox of `bbox` carried through `m` (column-major, affine).
fn voxelBox(bbox: *const DrawBox, m: *const [16]f32) [2][3]i32 {
    var lo = [3]f32{ std.math.floatMax(f32), std.math.floatMax(f32), std.math.floatMax(f32) };
    var hi = [3]f32{ -std.math.floatMax(f32), -std.math.floatMax(f32), -std.math.floatMax(f32) };
    for (0..8) |c| {
        const p = [3]f32{
            if (c & 1 != 0) bbox.hi[0] else bbox.lo[0],
            if (c & 2 != 0) bbox.hi[1] else bbox.lo[1],
            if (c & 4 != 0) bbox.hi[2] else bbox.lo[2],
        };
        for (0..3) |a| {
            const v = m[a] * p[0] + m[4 + a] * p[1] + m[8 + a] * p[2] + m[12 + a];
            lo[a] = util.fmin(lo[a], v);
            hi[a] = util.fmax(hi[a], v);
        }
    }
    var out: [2][3]i32 = undefined;
    for (0..3) |a| {
        out[0][a] = util.f2i(i32, @floor(lo[a] / 0.25 + 0.5));
        out[1][a] = util.f2i(i32, @floor(hi[a] / 0.25 + 0.5));
    }
    return out;
}

/// Voxels of the vertex box inside the grid box grown by one voxel.
fn boxOverlap(vlo: [3]i32, vhi: [3]i32, gmin: [3]i32, dims: [3]u32) i64 {
    var product: i64 = 1;
    for (0..3) |a| {
        const lo = @max(@as(i64, vlo[a]), @as(i64, gmin[a]) - 1);
        const hi = @min(@as(i64, vhi[a]), @as(i64, gmin[a]) + dims[a]);
        product *%= @max(hi - lo + 1, 0);
    }
    return product;
}

const Cand = struct {
    body: usize,
    min: [3]i32,
    dims: [3]u32,
    /// `inverse(body_mat) * comp` (identity for a matrix match).
    to_body: [16]f32,
    dist: f32,
};

fn candScore(c: *const Cand, bbox: ?*const DrawBox) i64 {
    const b = bbox orelse return 0;
    const vb = voxelBox(b, &c.to_body);
    return boxOverlap(vb[0], vb[1], c.min, c.dims);
}

/// Deterministic pick: largest bbox overlap, then nearest grid centre, then
/// lowest body pointer. Never recency. Returns the index into `cands`.
fn pick(cands: []const Cand, bbox: ?*const DrawBox) usize {
    var best: usize = 0;
    var best_s: i64 = std.math.minInt(i64);
    var best_d: f32 = std.math.floatMax(f32);
    var best_b: usize = std.math.maxInt(usize);
    for (cands, 0..) |*c, i| {
        const s = candScore(c, bbox);
        const better = s > best_s or (s == best_s and (c.dist < best_d or (c.dist == best_d and c.body < best_b)));
        if (better) {
            best = i;
            best_s = s;
            best_d = c.dist;
            best_b = c.body;
        }
    }
    return best;
}

fn logCandidates(ibo: u32, what: []const u8, cands: []const Cand, bbox: ?*const DrawBox, chosen: ?usize) void {
    if (!verbose) return;
    {
        const logged = CAND_LOGGED.lock();
        defer CAND_LOGGED.unlock();
        if (logged.items.len >= 256 or std.mem.indexOfScalar(u32, logged.items, ibo) != null) return;
        logged.append(gpa, ibo) catch return;
    }
    var text: std.ArrayList(u8) = .empty;
    defer text.deinit(gpa);
    for (cands, 0..) |*c, i| {
        if (i != 0) text.append(gpa, ' ') catch {};
        if (bbox) |b| {
            const vb = voxelBox(b, &c.to_body);
            text.print(gpa, "0x{x}(grid {any}+{any} overlap Some({d}))", .{ c.body, c.min, c.dims, boxOverlap(vb[0], vb[1], c.min, c.dims) }) catch {};
        } else {
            text.print(gpa, "0x{x}(grid {any}+{any} overlap None)", .{ c.body, c.min, c.dims }) catch {};
        }
    }
    var chosen_buf: [24]u8 = undefined;
    const chosen_text = if (chosen) |i| std.fmt.bufPrint(&chosen_buf, "0x{x}", .{cands[i].body}) catch "?" else "none";
    logf("FLIR: component ibo={d} {s}: bbox={f} candidates[{d}]: {s} -> chosen {s}", .{
        ibo, what, OptBox{ .v = if (bbox) |b| b.* else null }, cands.len, text.items, chosen_text,
    });
}

fn sortByBody(cands: []Cand) void {
    std.mem.sort(Cand, cands, {}, struct {
        fn lessThan(_: void, a: Cand, b: Cand) bool {
            return a.body < b.body;
        }
    }.lessThan);
}

const Resolved = struct { body: usize, to_body: [16]f32 };

/// Find the body for a component/unmapped draw. Returns the body and
/// `inverse(body_mat) * comp_mat`. When several bodies share the draw's matrix
/// (one vehicle frame) or contain its origin, the body whose grid overlaps the
/// draw's vertex bbox the most is chosen; the choice never depends on order.
fn resolveComponent(g: *const Gl, program: u32, ibo: u32, count: i32, indices: ?*const anyopaque, comp: *const [16]f32) ?Resolved {
    const view = ACTIVE_VIEW.load(.monotonic);
    const first = @intFromPtr(indices) / 2;
    const t = [3]f32{ comp[12], comp[13], comp[14] };
    var exact: std.ArrayList(Cand) = .empty;
    defer exact.deinit(gpa);
    var near: std.ArrayList(Cand) = .empty;
    defer near.deinit(gpa);
    {
        const mats = BODY_MATS.lock();
        defer BODY_MATS.unlock();
        // Locked only once a body is close: most unmapped draws are terrain
        // and props far from every vehicle (~100k draws/s with FLIR on).
        var grids: ?*fx.HashMap(usize, GridTexture) = null;
        defer if (grids != null) GRID_TEXTURES.unlock();
        var taken: usize = 0;
        for (mats.items) |e| {
            if (e.view != view) continue;
            if (taken >= MAX_BODY_MATS) break;
            taken += 1;
            const d = [3]f32{ t[0] - e.mat[12], t[1] - e.mat[13], t[2] - e.mat[14] };
            const d2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
            // Cheap sphere reject (an exact matrix match has d = 0): the body
            // matrix is rigid, so a draw origin farther from the body origin
            // than the grid's farthest corner (+1 voxel) cannot be inside it.
            if (d2 > e.reach2) continue;
            if (grids == null) grids = GRID_TEXTURES.lock();
            const grid = grids.?.get(e.body) orelse continue;
            var same = true;
            for (0..3) |i| {
                if (@abs(e.mat[12 + i] - t[i]) > 1e-3) same = false;
            }
            for (0..12) |i| {
                if (@abs(e.mat[i] - comp[i]) > 1e-3) same = false;
            }
            if (same) {
                exact.append(gpa, .{ .body = e.body, .min = grid.min, .dims = grid.dims, .to_body = IDENTITY, .dist = 0 }) catch {};
                continue;
            }
            if (d2 > gridReach2(grid.min, grid.dims)) continue;
            const inv = invAffine(&e.mat) orelse continue;
            var dist: f32 = 0;
            var inside = true;
            for (0..3) |i| {
                var p: f32 = 0;
                for (0..3) |j| p += inv[j * 4 + i] * d[j];
                p /= 0.25;
                const v = util.f2i(i32, @round(p));
                const lo = grid.min[i];
                const hi = grid.min[i] + @as(i32, @intCast(grid.dims[i])) - 1;
                inside = inside and v >= lo - 1 and v <= hi + 1;
                const centre = util.f32_(lo + hi) * 0.5;
                dist += (p - centre) * (p - centre);
            }
            if (inside) near.append(gpa, .{ .body = e.body, .min = grid.min, .dims = grid.dims, .to_body = matMul(&inv, comp), .dist = dist }) catch {};
        }
    }
    if (exact.items.len > 0) {
        sortByBody(exact.items);
        if (exact.items.len == 1) return .{ .body = exact.items[0].body, .to_body = IDENTITY };
        var set: u64 = 0xcbf2_9ce4_8422_2325;
        for (exact.items) |c| set = (set ^ @as(u64, c.body)) *% 0x100_0000_01b3;
        const key: ChoiceKey = .{ .ibo = ibo, .first = first, .count = count, .set = set };
        const cached = blk: {
            const choice = BODY_CHOICE.lock();
            defer BODY_CHOICE.unlock();
            break :blk choice.get(key);
        };
        if (cached) |body| {
            for (exact.items) |c| if (c.body == body) return .{ .body = body, .to_body = IDENTITY };
        }
        const bbox = drawBbox(g, program, ibo, count, indices) orelse return null;
        const bbox_ptr: ?*const DrawBox = if (bbox) |*b| b else null;
        const chosen = pick(exact.items, bbox_ptr);
        logCandidates(ibo, "matrix-match", exact.items, bbox_ptr, chosen);
        const body = exact.items[chosen].body;
        {
            const choice = BODY_CHOICE.lock();
            defer BODY_CHOICE.unlock();
            if (choice.count() >= MAX_BOX_CACHE) choice.clearRetainingCapacity();
            choice.put(gpa, key, body) catch {};
        }
        return .{ .body = body, .to_body = IDENTITY };
    }
    if (near.items.len == 0) {
        if (verbose) logUnresolved(g, program, ibo, count, indices, comp, view, t);
        return null;
    }
    sortByBody(near.items);
    var chosen: usize = 0;
    if (near.items.len > 1) {
        const bbox = drawBbox(g, program, ibo, count, indices) orelse return null;
        const bbox_ptr: ?*const DrawBox = if (bbox) |*b| b else null;
        chosen = pick(near.items, bbox_ptr);
        logCandidates(ibo, "origin-in-grid", near.items, bbox_ptr, chosen);
    }
    const c = near.items[chosen];
    if (verbose) {
        const logged = COMP_LOGGED.lock();
        defer COMP_LOGGED.unlock();
        if (logged.items.len < 32 and std.mem.indexOfScalar(u32, logged.items, ibo) == null) {
            logged.append(gpa, ibo) catch {};
            var voxel: [3]i32 = undefined;
            for (0..3) |i| voxel[i] = util.f2i(i32, @round(c.to_body[12 + i] / 0.25));
            logf("FLIR: component draw ibo={d} -> body 0x{x} voxel {any}", .{ ibo, c.body, voxel });
        }
    }
    return .{ .body = c.body, .to_body = c.to_body };
}

/// Unresolved: log every body with a grid and where the draw lands in it. Only
/// while the diagnostic can still log: past the cap every new terrain IBO cost
/// a stalling GPU readback (2 per frame, forever).
fn logUnresolved(g: *const Gl, program: u32, ibo: u32, count: i32, indices: ?*const anyopaque, comp: *const [16]f32, view: u32, t: [3]f32) void {
    {
        const logged = CAND_LOGGED.lock();
        defer CAND_LOGGED.unlock();
        if (logged.items.len >= 256 or std.mem.indexOfScalar(u32, logged.items, ibo) != null) return;
    }
    const bbox = drawBbox(g, program, ibo, count, indices) orelse return;
    var all: std.ArrayList(Cand) = .empty;
    defer all.deinit(gpa);
    {
        const mats = BODY_MATS.lock();
        defer BODY_MATS.unlock();
        const grids = GRID_TEXTURES.lock();
        defer GRID_TEXTURES.unlock();
        for (mats.items) |e| {
            if (e.view != view) continue;
            const grid = grids.get(e.body) orelse continue;
            const inv = invAffine(&e.mat) orelse continue;
            all.append(gpa, .{ .body = e.body, .min = grid.min, .dims = grid.dims, .to_body = matMul(&inv, comp), .dist = 0 }) catch {};
        }
    }
    var what: [96]u8 = undefined;
    const label = std.fmt.bufPrint(&what, "UNRESOLVED (origin {any}) all bodies", .{t}) catch "UNRESOLVED all bodies";
    logCandidates(ibo, label, all.items, if (bbox) |*b| b else null, null);
}

// ---------------------------------------------------------------------------
// Skinned draws (characters): warm-body marker
// ---------------------------------------------------------------------------

/// Light factor written into gnormal.w by a skinned draw in a FLIR view: the
/// composite reads it as a warm body (people, animals). The game's light pass
/// thresholds the factor (< 0.25 -> 0), and only views we replace see it.
const WARM_BODY_MARKER: f32 = -3.0;
const SKINNED_SLOTS: usize = 8;
/// (program, light_factor location, game's last light_factor bits) per skinned
/// program; program 0 = free. Lock-free: the glUniform1f hook runs per call.
const SkinnedSlot = struct {
    program: Atomic(u32) = .init(0),
    light_factor: Atomic(i32) = .init(-1),
    last_bits: Atomic(u32) = .init(0x3f80_0000),
};
var SKINNED: [SKINNED_SLOTS]SkinnedSlot = [_]SkinnedSlot{.{}} ** SKINNED_SLOTS;
var ORIG_GL_UNIFORM1F: Atomic(usize) = .init(0);
/// Light-factor location to put back after the forwarded skinned draw (-1 none).
var RESTORE_LF: Atomic(i32) = .init(-1);
var RESTORE_LF_BITS: Atomic(u32) = .init(0);

fn registerSkinned(program: u32, light_factor: i32) void {
    if (light_factor < 0) return;
    for (&SKINNED) |*slot| {
        if (slot.program.load(.monotonic) == program) return;
        if (slot.program.cmpxchgStrong(0, program, .monotonic, .monotonic) == null) {
            slot.light_factor.store(light_factor, .monotonic);
            return;
        }
    }
}

fn skinnedSlot(program: u32) ?*SkinnedSlot {
    for (&SKINNED) |*slot| if (slot.program.load(.monotonic) == program) return slot;
    return null;
}

fn trackedProgram() ?u32 {
    const program = TRACK_PROGRAM.load(.monotonic);
    return if (program > 0) @truncate(@as(u64, @bitCast(program))) else null;
}

/// `glUniform1f` thunk: remembers the game's own light_factor for skinned
/// programs so the marker can be undone after the draw.
fn glUniform1fHook(location: i32, value: f32) callconv(API) void {
    const original = ORIG_GL_UNIFORM1F.load(.monotonic);
    if (original != 0) {
        const f: *const fn (i32, f32) callconv(API) void = @ptrFromInt(original);
        f(location, value);
    }
    if (trackedProgram()) |program| recordParticleUniform(program, location, value);
    if (SKINNED[0].program.load(.monotonic) != 0) {
        if (trackedProgram()) |program| {
            if (skinnedSlot(program)) |slot| {
                if (slot.light_factor.load(.monotonic) == location) slot.last_bits.store(@bitCast(value), .monotonic);
            }
        }
    }
}

/// In a FLIR G-buffer pass, mark a skinned draw as a warm body (set its
/// light_factor); the forwarded game draw keeps its own animation.
fn markSkinnedDraw(g: *const Gl, program: u32) void {
    const slot = skinnedSlot(program) orelse return;
    const loc = slot.light_factor.load(.monotonic);
    if (loc < 0) return;
    g.uniform1f(loc, WARM_BODY_MARKER);
    RESTORE_LF_BITS.store(slot.last_bits.load(.monotonic), .monotonic);
    RESTORE_LF.store(loc, .monotonic);
}

/// Undo `markSkinnedDraw` after the game's draw (same program bound).
fn restoreSkinnedDraw() void {
    const loc = RESTORE_LF.swap(-1, .monotonic);
    if (loc >= 0) {
        if (gl()) |g| g.uniform1f(loc, @bitCast(RESTORE_LF_BITS.load(.monotonic)));
    }
}

/// gnormal.w of a hot-gas particle in a FLIR G-buffer; gcolor.r = excess / 1000 C.
const GAS_MARKER: f32 = -5.0;

/// Particle programs redrawn hot in a FLIR G-buffer, keeping the game's own
/// noise cut-out so each puff keeps its shape.
const PK_DISSOLVE: u32 = 0; // smoke_flare_disolve: exhaust, tyre/impact smoke, spray, steam
const PK_FLARE: u32 = 1; // flare (HEAT 0/1): flares and burning-fire flames
const PK_FIRE: u32 = 2; // fire: explosion fireballs, muzzle blast, lava
const PK_PLUME: u32 = 3; // flare without HEAT_ENABLED: exhaust plume billboards

const PartProg = struct {
    program: u32,
    kind: u32,
    /// Game attribute locations: pos, normal, coord/uv, colour, previous pos.
    attrs: [5]i32,
    noise_unit: i32,
    /// Game uniform locations of interp_factor / timer (glUniform1f).
    uni_locs: [2]i32,
};

var PARTS: Mutex(std.ArrayList(PartProg)) = .{ .value = .empty };
/// Lock-free program ids of PARTS for the glUniform1f hook.
var PART_IDS: [8]Atomic(u32) = [_]Atomic(u32){.init(0)} ** 8;
/// Last interp_factor / timer per PARTS slot (f32 bits).
var PART_UNI: [8][2]Atomic(u32) = [_][2]Atomic(u32){[_]Atomic(u32){.init(0)} ** 2} ** 8;
/// Built FLIR particle programs per (kind, attrs).
const PartGl = struct { program: u32, vp: i32, world: i32, noise: i32, interp: i32, timer: i32, air: i32 };
const PartKey = struct { kind: u32, attrs: [5]i32 };
const PartProgramEntry = struct { key: PartKey, built: ?PartGl };
var PART_PROGRAMS: Mutex(std.ArrayList(PartProgramEntry)) = .{ .value = .empty };

fn registerParticle(p: PartProg) void {
    const parts = PARTS.lock();
    defer PARTS.unlock();
    if (parts.items.len >= 8) return;
    for (parts.items) |e| if (e.program == p.program) return;
    PART_IDS[parts.items.len].store(p.program, .monotonic);
    parts.append(gpa, p) catch return;
    logDebug("FLIR: game program {d} is a particle program kind={d} (0 smoke, 1 flare, 2 fire) attrs={any} noise_unit={d} uni={any}", .{ p.program, p.kind, p.attrs, p.noise_unit, p.uni_locs });
}

fn particleSlot(program: u32) ?struct { slot: usize, prog: PartProg } {
    if (PART_IDS[0].load(.monotonic) == 0) return null;
    var any = false;
    for (&PART_IDS) |*id| if (id.load(.monotonic) == program) {
        any = true;
    };
    if (!any) return null;
    const parts = PARTS.lock();
    defer PARTS.unlock();
    for (parts.items, 0..) |e, i| if (e.program == program) return .{ .slot = i, .prog = e };
    return null;
}

/// glUniform1f hook side: remember a particle program's interp_factor/timer.
fn recordParticleUniform(program: u32, location: i32, value: f32) void {
    if (PART_IDS[0].load(.monotonic) == 0) return;
    const i = for (&PART_IDS, 0..) |*id, i| {
        if (id.load(.monotonic) == program) break i;
    } else return;
    const p = blk: {
        const parts = PARTS.lock();
        defer PARTS.unlock();
        if (i >= parts.items.len) return;
        break :blk parts.items[i];
    };
    for (0..2) |k| {
        if (p.uni_locs[k] >= 0 and p.uni_locs[k] == location) PART_UNI[i][k].store(@bitCast(value), .monotonic);
    }
}

const PART_VS = @embedFile("shaders/particle.vert");
const PART_FS = @embedFile("shaders/particle.frag");

/// The particle shader pair for one kind (`#define KIND n` prepended, as the
/// game prepends its own #defines).
fn particleSources(kind: u32) ?struct { vs: [:0]const u8, fs: [:0]const u8 } {
    inline for (0..4) |k| {
        if (kind == k) {
            const head = std.fmt.comptimePrint("#version 330 core\n#define KIND {d}\n", .{k});
            return .{ .vs = head ++ PART_VS, .fs = head ++ PART_FS };
        }
    }
    return null;
}

/// Whether any attached shader's source contains `needle` (the game prepends
/// its #defines to the source). Null = sources unavailable. Setup-time query.
fn programDefines(g: *const Gl, program: u32, needle: []const u8) ?bool {
    var shaders = [4]u32{ 0, 0, 0, 0 };
    var n: i32 = 0;
    g.get_attached_shaders(program, 4, &n, &shaders);
    var seen = false;
    const buf = gpa.alloc(u8, 65536) catch return null;
    defer gpa.free(buf);
    var flat: std.ArrayList(u8) = .empty;
    defer flat.deinit(gpa);
    for (shaders[0..@intCast(std.math.clamp(n, 0, 4))]) |shader| {
        var len: i32 = 0;
        g.get_shader_source(shader, @intCast(buf.len), &len, buf.ptr);
        if (len <= 0) continue;
        seen = true;
        // Tolerate "#define HEAT_ENABLED 1" with any spacing.
        flat.clearRetainingCapacity();
        var words = std.mem.tokenizeAny(u8, buf[0..@min(@as(usize, @intCast(len)), buf.len)], " \t\r\n");
        while (words.next()) |word| {
            if (flat.items.len > 0) flat.append(gpa, ' ') catch {};
            flat.appendSlice(gpa, word) catch {};
        }
        if (std.mem.indexOf(u8, flat.items, needle) != null) return true;
    }
    return if (seen) false else null;
}

const PartAttribBinder = struct {
    attrs: [5]i32,
    fn bind(self: PartAttribBinder, g: *const Gl, program: u32) void {
        const names = [5][:0]const u8{ "vertex_position_in", "vertex_normal_in", "vertex_uv_in", "vertex_color_in", "vertex_bone_weights_in" };
        for (self.attrs, names) |loc, name| {
            if (loc >= 0) g.bind_attrib_location(program, @intCast(loc), name);
        }
        g.bind_frag_data_location(program, 0, "flir_out0");
        g.bind_frag_data_location(program, 1, "flir_out1");
        g.bind_frag_data_location(program, 2, "flir_out2");
    }
};

fn particleProgram(g: *const Gl, p: *const PartProg) ?PartGl {
    const key: PartKey = .{ .kind = p.kind, .attrs = p.attrs };
    const cache = PART_PROGRAMS.lock();
    defer PART_PROGRAMS.unlock();
    for (cache.items) |e| if (std.meta.eql(e.key, key)) return e.built;
    const sources = particleSources(p.kind) orelse return null;
    const built: ?PartGl = if (linkProgram(g, sources.vs, sources.fs, PartAttribBinder{ .attrs = p.attrs }, PartAttribBinder.bind)) |program| .{
        .program = program,
        .vp = uniform(g, program, "mat_view_proj"),
        .world = uniform(g, program, "mat_world"),
        .noise = uniform(g, program, "texture_noise0"),
        .interp = uniform(g, program, "interp_factor"),
        .timer = uniform(g, program, "timer"),
        .air = uniform(g, program, "air_c"),
    } else null;
    if (built == null) logDebug("FLIR: particle program kind={d} failed to build; game draws kept", .{p.kind});
    if (cache.items.len < 16) cache.append(gpa, .{ .key = key, .built = built }) catch {};
    return built;
}

/// Redraw a particle batch as hot gas. False = keep the game draw.
fn particleDraw(g: *const Gl, slot: usize, p: PartProg, mode: u32, count: i32, kind: u32, indices: ?*const anyopaque) bool {
    const game = p.program;
    const pm = blk: {
        const mats = PROG_MATS.lock();
        defer PROG_MATS.unlock();
        break :blk mats.get(game);
    };
    const mats = pm orelse ProgMats{};
    if (mats.vp == null or mats.world == null) {
        logOnceKey(0x6a5_0004, "FLIR particles: program {d} has no recorded matrices; game draw kept", .{game});
        return false;
    }
    const vp = mats.vp.?;
    const w = mats.world.?;
    const pg = particleProgram(g, &p) orelse return false;
    logOnceKey(0x6a5_0005 + @as(u64, p.kind) * 0x100, "FLIR particles: first substituted draw kind={d} program={d} count={d}", .{ p.kind, game, count });
    IN_SUBSTITUTE.store(true, .release);
    defer IN_SUBSTITUTE.store(false, .release);
    _ = tracked(g, &TRACK_ACTIVE_TEX, GL_ACTIVE_TEXTURE);
    g.use_program(pg.program);
    g.uniform_matrix4fv(pg.vp, 1, vp.transpose, &vp.m);
    g.uniform_matrix4fv(pg.world, 1, w.transpose, &w.m);
    g.uniform1i(pg.noise, @max(p.noise_unit, 0));
    if (pg.interp >= 0) g.uniform1f(pg.interp, @bitCast(PART_UNI[slot][0].load(.monotonic)));
    if (pg.timer >= 0) g.uniform1f(pg.timer, @bitCast(PART_UNI[slot][1].load(.monotonic)));
    // Same air the composite uses for the main view (feeds differ by a
    // degree or two at most).
    if (pg.air >= 0) g.uniform1f(pg.air, sceneAmbient());
    g.draw_elements(mode, count, kind, indices);
    g.use_program(game);
    return true;
}

/// A program is treated as the game's opaque pass (or one of its variants) when
/// it exposes the opaque attribute/uniform layout. Only checked for a draw whose
/// IBO is already mapped to a body, so the query cost is paid once per program.
fn opaqueProgram(g: *const Gl, program: u32) bool {
    {
        const programs = OPAQUE_PROGRAMS.lock();
        defer OPAQUE_PROGRAMS.unlock();
        if (programs.get(program)) |known| return known;
    }
    const attr = struct {
        fn of(gg: *const Gl, prog: u32, name: [*:0]const u8) i32 {
            glq(@src());
            return gg.get_attrib_location(prog, name);
        }
    }.of;
    const uni = struct {
        fn of(gg: *const Gl, prog: u32, name: [*:0]const u8) i32 {
            glq(@src());
            return gg.get_uniform_location(prog, name);
        }
    }.of;
    const pos = attr(g, program, "vertex_position_in");
    const norm = attr(g, program, "vertex_normal_in");
    const world = uni(g, program, "mat_world");
    if (pos < 0 or norm < 0 or world < 0) {
        // smoke_flare lands here too (normal optimised out). RE (re_clouds):
        // its only CPU pushers are the weather clouds, so the game draw is
        // kept and clouds shade like the rest of the scene.
        logRejectedProgram(program, pos, norm, world);
        rememberOpaque(program, false);
        return false;
    }
    // Skinned (characters: bone matrices) and vertex-animated programs move
    // their vertices in the vertex shader; the block VS would draw the bind
    // pose (the T-posing player touching a vehicle). Never substitute them.
    const bones = attr(g, program, "vertex_bone_weights_in");
    const animated = attr(g, program, "vertex_velocity_in");
    // smoke_flare_disolve (exhaust/impact smoke puffs, batched): noise-
    // dissolved billboards whose vertex_normal.z is the puff's remaining
    // life. Redrawn as hot gas, never as vehicle blocks.
    const noise = uni(g, program, "texture_noise0");
    if (noise >= 0) {
        const coord = attr(g, program, "vertex_coord_in");
        const uvloc = attr(g, program, "vertex_uv_in");
        const colour = attr(g, program, "vertex_color_in");
        const interp = uni(g, program, "interp_factor");
        const timer = uni(g, program, "timer");
        // foam_particle (ocean foam) also has noise + timer + uv, plus
        // light_factor/texture_falloff, which fire.glslf lacks.
        const foam = uni(g, program, "light_factor") >= 0 or uni(g, program, "texture_falloff") >= 0;
        var kind: ?u32 = null;
        var uv: i32 = -1;
        if (coord >= 0 and bones < 0) {
            kind = PK_DISSOLVE;
            uv = coord;
        } else if (bones >= 0 and interp >= 0 and uvloc >= 0) {
            // flare is compiled twice: HEAT_ENABLED=1 draws glowing
            // flares/flames, the plain variant smoke-like plumes (jet
            // exhaust): hot, cooling with the particle's life.
            kind = if (programDefines(g, program, "HEAT_ENABLED 1") == false) PK_PLUME else PK_FLARE;
            uv = uvloc;
        } else if (bones < 0 and timer >= 0 and uvloc >= 0 and !foam) {
            kind = PK_FIRE;
            uv = uvloc;
        }
        if (kind) |k| {
            // The sampler's unit is assigned once at shader setup (0 in
            // every particle pass per the RE notes).
            var unit: i32 = 0;
            g.get_uniformiv(program, noise, @ptrCast(&unit));
            registerParticle(.{
                .program = program,
                .kind = k,
                .attrs = .{ pos, norm, uv, if (k == PK_DISSOLVE) colour else -1, if (k == PK_FLARE or k == PK_PLUME) bones else -1 },
                .noise_unit = unit,
                .uni_locs = .{ interp, timer },
            });
            rememberOpaque(program, false);
            return false;
        }
    }
    if (bones >= 0 or animated >= 0) {
        if (bones >= 0) registerSkinned(program, uni(g, program, "light_factor"));
        logDebug("FLIR: game program {d} is skinned/animated; never substituted", .{program});
        rememberOpaque(program, false);
        return false;
    }
    rememberOpaque(program, true);
    logDebug("FLIR: game program {d} cached as an opaque pass (block substitution)", .{program});
    return true;
}

fn rememberOpaque(program: u32, opaque_pass: bool) void {
    const programs = OPAQUE_PROGRAMS.lock();
    defer OPAQUE_PROGRAMS.unlock();
    if (programs.count() >= 1024) programs.clearRetainingCapacity();
    programs.put(gpa, program, opaque_pass) catch {};
}

var REJECTED_SEEN: Mutex(std.ArrayList(u32)) = .{ .value = .empty };

/// One-shot per rejected program: which opaque-layout pieces it lacks and its
/// fragment outputs (GL 4.3 program interface, optional entry points).
fn logRejectedProgram(program: u32, pos: i32, norm: i32, world: i32) void {
    if (!verbose) return;
    {
        const seen = REJECTED_SEEN.lock();
        defer REJECTED_SEEN.unlock();
        if (seen.items.len >= 16 or std.mem.indexOfScalar(u32, seen.items, program) != null) return;
        seen.append(gpa, program) catch return;
    }
    var outs: std.ArrayList(u8) = .empty;
    defer outs.deinit(gpa);
    const GL_PROGRAM_OUTPUT: u32 = 0x92e4;
    const GL_ACTIVE_RESOURCES: u32 = 0x92f5;
    const iface_raw = glSymbol("glGetProgramInterfaceiv");
    const name_raw = glSymbol("glGetProgramResourceName");
    const loc_raw = glSymbol("glGetProgramResourceLocation");
    if (iface_raw != 0 and name_raw != 0 and loc_raw != 0) {
        const iface: *const fn (u32, u32, u32, *i32) callconv(API) void = @ptrFromInt(iface_raw);
        const name_fn: *const fn (u32, u32, u32, i32, *i32, [*]u8) callconv(API) void = @ptrFromInt(name_raw);
        const loc_fn: *const fn (u32, u32, [*:0]const u8) callconv(API) i32 = @ptrFromInt(loc_raw);
        var n: i32 = 0;
        iface(program, GL_PROGRAM_OUTPUT, GL_ACTIVE_RESOURCES, &n);
        var i: u32 = 0;
        while (i < @as(u32, @intCast(std.math.clamp(n, 0, 16)))) : (i += 1) {
            var buf = [_]u8{0} ** 64;
            var len: i32 = 0;
            name_fn(program, GL_PROGRAM_OUTPUT, i, 64, &len, &buf);
            buf[63] = 0;
            const loc = loc_fn(program, GL_PROGRAM_OUTPUT, @ptrCast(&buf));
            outs.print(gpa, " {s}@{d}", .{ buf[0..@intCast(std.math.clamp(len, 0, 63))], loc }) catch {};
        }
    } else {
        outs.appendSlice(gpa, " (program-interface query unavailable)") catch {};
    }
    logf("FLIR: program {d} rejected as non-opaque: vertex_position_in={d} vertex_normal_in={d} mat_world={d}; fragment outputs:{s}", .{ program, pos, norm, world, outs.items });
}

/// Bind the body temperature texture and the FLIR block program. Returns the
/// saved GL state to restore after the draw, or null to fall back.
fn beginBlockDraw(g: *const Gl, body: usize, ibo: u32, count: i32, indices: ?*const anyopaque, normal_encoded: bool, comp_to_body: *const [16]f32) ?SavedState {
    const view_proj = blk: {
        const m = VIEW_PROJ.lock();
        defer VIEW_PROJ.unlock();
        break :blk m.*;
    };
    const world = blk: {
        const m = WORLD_MATRIX.lock();
        defer WORLD_MATRIX.unlock();
        break :blk m.*;
    };
    const view_proj_transpose: u8 = @truncate(VIEW_PROJ_TRANSPOSE.load(.monotonic));
    const world_transpose: u8 = @truncate(WORLD_TRANSPOSE.load(.monotonic));

    const program = tracked(g, &TRACK_PROGRAM, GL_CURRENT_PROGRAM);
    if (program <= 0) return null;
    const block = blockProgram(g, @intCast(program)) orelse return null;

    const active = tracked(g, &TRACK_ACTIVE_TEX, GL_ACTIVE_TEXTURE);
    g.active_texture(GL_TEXTURE0 + offsets.FLIR_TEXTURE_UNIT);
    // No game shader samples a 3D texture (no sampler3D in rom/graphics/shaders),
    // so unit 7's 3D binding is ours alone (tracked in LAST_3D, never queried).
    const entry = bodyTexture(g, body) orelse {
        diagCount(DIAG_NO_GRID);
        g.active_texture(@bitCast(active));
        return null;
    };
    // One line, written and flushed BEFORE the substitute draw: if the first
    // activation is what kills the game, the log already names the body, grid,
    // texture and program it was about to draw with (`logLine` flushes).
    logOnce(LOG_FLIR_BLOCK, "FLIR: block pass active: body=0x{x} ibo={d} grid_min={any} dims={any} texture={d} program={d} game_program={d} count={d} (substitute draw follows)", .{
        body, ibo, entry.min, entry.dims, entry.texture, block.program, program, count,
    });
    if (verbose) vertexDiag(g, body, ibo, @intCast(program), count, indices, entry.min, entry.dims);
    if (LAST_3D.swap(entry.texture, .monotonic) != entry.texture) g.bind_texture(GL_TEXTURE_3D, entry.texture);

    g.use_program(block.program);
    // Component origin in body-local voxels (identity = chunk draw: none).
    var comp_voxel: [3]i32 = undefined;
    for (0..3) |i| comp_voxel[i] = util.f2i(i32, @round(comp_to_body[12 + i] / 0.25));
    const cur: BlockUni = .{
        .vp = .{ .m = view_proj, .transpose = view_proj_transpose },
        .world = .{ .m = world, .transpose = world_transpose },
        .comp = comp_to_body.*,
        .min = entry.min,
        .dims = entry.dims,
        .normal_encoded = normal_encoded,
        .comp_voxel = comp_voxel,
        .is_comp = !std.mem.eql(f32, comp_to_body, &IDENTITY),
    };
    const cache = BLOCK_UNI.lock();
    defer BLOCK_UNI.unlock();
    var slot: ?usize = null;
    for (cache.items, 0..) |e, i| {
        if (e.program == block.program) {
            slot = i;
            break;
        }
    }
    const last: ?BlockUni = if (slot) |i| cache.items[i].uni else null;
    if (last == null) {
        g.uniform1i(block.tex_temps, offsets.FLIR_TEXTURE_UNIT);
        g.uniform1f(block.enc_min, offsets.FLIR_ENC_MIN_C);
        g.uniform1f(block.enc_max, offsets.FLIR_ENC_MAX_C);
        if (block.has_color >= 0) g.uniform1i(block.has_color, @intFromBool(block.color_loc >= 0));
    }
    // The matrices are replayed exactly as the game uploaded them (raw floats
    // and the game's transpose flag), so the transform is identical.
    if (last == null or !std.meta.eql(last.?.vp, cur.vp)) g.uniform_matrix4fv(block.mat_view_proj, 1, view_proj_transpose, &view_proj);
    if (last == null or !std.meta.eql(last.?.world, cur.world)) g.uniform_matrix4fv(block.mat_world, 1, world_transpose, &world);
    if (last == null or !std.meta.eql(last.?.comp, cur.comp)) g.uniform_matrix4fv(block.comp_to_body, 1, 0, comp_to_body);
    if (last == null or !std.meta.eql(last.?.min, cur.min)) g.uniform3i(block.grid_min, entry.min[0], entry.min[1], entry.min[2]);
    if (last == null or !std.meta.eql(last.?.dims, cur.dims)) g.uniform3i(block.grid_dims, @intCast(entry.dims[0]), @intCast(entry.dims[1]), @intCast(entry.dims[2]));
    // The attachment's component type decides the normal packing and which
    // channel carries the vehicle marker (see block.frag).
    if (last == null or last.?.normal_encoded != cur.normal_encoded) g.uniform1i(block.normal_encoded, @intFromBool(normal_encoded));
    if (block.comp_voxel >= 0 and block.has_comp_voxel >= 0) {
        if (last == null or !std.meta.eql(last.?.comp_voxel, cur.comp_voxel)) g.uniform3i(block.comp_voxel, comp_voxel[0], comp_voxel[1], comp_voxel[2]);
        if (last == null or last.?.is_comp != cur.is_comp) g.uniform1i(block.has_comp_voxel, @intFromBool(cur.is_comp));
    }
    if (slot) |i| {
        cache.items[i].uni = cur;
    } else {
        cache.append(gpa, .{ .program = block.program, .uni = cur }) catch {};
    }
    return .{ .program = program, .active_texture = active };
}

var VERTEX_DIAG_DONE: Mutex(std.ArrayList(usize)) = .{ .value = .empty };

/// One-shot per body: read back the vertices the draw really uses and compare
/// their voxel coordinates (shader formula, normal ignored) with the grid.
fn vertexDiag(g: *const Gl, body: usize, ibo: u32, program: u32, count: i32, indices: ?*const anyopaque, gmin: [3]i32, dims: [3]u32) void {
    {
        const done = VERTEX_DIAG_DONE.lock();
        defer VERTEX_DIAG_DONE.unlock();
        if (done.items.len >= 16 or std.mem.indexOfScalar(usize, done.items, body) != null) return;
        done.append(gpa, body) catch return;
    }
    const a = positionAttrib(g, program) orelse return;
    const first = @intFromPtr(indices) / 2;
    var head_buf: [512]u8 = undefined;
    const head = std.fmt.bufPrint(&head_buf, "FLIR diag: body=0x{x} ibo={d} vbo={d} attrib(loc={d} size={d} type=0x{x} stride={d} normalized={d} offset={d}) draw first={d} count={d} grid_min={any} dims={any}", .{
        body, ibo, a.vbo, a.loc, a.size, a.kind, a.stride, a.norm, a.offset, first, count, gmin, dims,
    }) catch "FLIR diag";
    if (@as(u32, @bitCast(a.kind)) != GL_FLOAT or a.size < 3 or a.vbo <= 0) {
        logf("{s}: vertex type not float3+, skipped", .{head});
        return;
    }
    // Sample indices from the element buffer (already bound by the game).
    const n: usize = @min(@as(usize, @intCast(@max(count, 0))), 512);
    var idx_buf: [512]u16 = undefined;
    const idx = idx_buf[0..n];
    @memset(idx, 0);
    glq(@src());
    g.get_buffer_sub_data(ELEMENT_ARRAY_BUFFER, @intCast(first * 2), @intCast(n * 2), idx.ptr);
    var imin: u16 = std.math.maxInt(u16);
    var imax: u16 = 0;
    for (idx) |i| {
        imin = @min(imin, i);
        imax = @max(imax, i);
    }
    const step: usize = if (a.stride > 0) @intCast(a.stride) else @as(usize, @intCast(a.size)) * 4;
    var prev_array: i32 = 0;
    glq(@src());
    g.get_integerv(ARRAY_BUFFER_BINDING, @ptrCast(&prev_array));
    g.bind_buffer(ARRAY_BUFFER, @intCast(a.vbo));
    var bsize: i32 = 0;
    glq(@src());
    g.get_buffer_parameteriv(ARRAY_BUFFER, BUFFER_SIZE, &bsize);
    const span = @min((@as(usize, imax) -| imin) + 1, 2048);
    const start = a.offset + @as(usize, imin) * step;
    const want = span * step;
    const data = gpa.alloc(u8, want) catch {
        g.bind_buffer(ARRAY_BUFFER, @bitCast(prev_array));
        return;
    };
    defer gpa.free(data);
    const ok = n > 0 and start + want <= @as(usize, @intCast(@max(bsize, 0)));
    if (ok) {
        glq(@src());
        g.get_buffer_sub_data(ARRAY_BUFFER, @intCast(start), @intCast(want), data.ptr);
    }
    g.bind_buffer(ARRAY_BUFFER, @bitCast(prev_array));
    if (!ok) {
        logf("{s}: indices {d}..{d} need bytes {d}+{d} but vbo size={d}; skipped", .{ head, imin, imax, start, want, bsize });
        return;
    }
    var vox: std.ArrayList([3]i32) = .empty;
    defer vox.deinit(gpa);
    var raw_min = [3]f32{ std.math.floatMax(f32), std.math.floatMax(f32), std.math.floatMax(f32) };
    var raw_max = [3]f32{ -std.math.floatMax(f32), -std.math.floatMax(f32), -std.math.floatMax(f32) };
    for (idx) |i| {
        const rel = @as(usize, i) -% imin;
        if (rel >= span) continue;
        const o = rel * step;
        if (o + 12 > data.len) continue;
        var v: [3]i32 = undefined;
        for (0..3) |axis| {
            const f = readF32Le(data, o + axis * 4);
            raw_min[axis] = util.fmin(raw_min[axis], f);
            raw_max[axis] = util.fmax(raw_max[axis], f);
            v[axis] = util.f2i(i32, @floor(f / 0.25 + 0.5));
        }
        vox.append(gpa, v) catch {};
    }
    if (vox.items.len == 0) {
        logf("{s}: no vertices decoded", .{head});
        return;
    }
    var vmin = [3]i32{ std.math.maxInt(i32), std.math.maxInt(i32), std.math.maxInt(i32) };
    var vmax = [3]i32{ std.math.minInt(i32), std.math.minInt(i32), std.math.minInt(i32) };
    for (vox.items) |v| {
        for (0..3) |axis| {
            vmin[axis] = @min(vmin[axis], v[axis]);
            vmax[axis] = @max(vmax[axis], v[axis]);
        }
    }
    var gmax: [3]i32 = undefined;
    for (0..3) |axis| gmax[axis] = gmin[axis] + @as(i32, @intCast(dims[axis])) - 1;
    const Fit = struct {
        fn inside(points: []const [3]i32, perm: [3]usize, sign: [3]i32, lo: [3]i32, hi: [3]i32) usize {
            var c: usize = 0;
            outer: for (points) |v| {
                for (0..3) |axis| {
                    const val = sign[axis] *% v[perm[axis]];
                    if (val < lo[axis] or val > hi[axis]) continue :outer;
                }
                c += 1;
            }
            return c;
        }
    };
    const perms = [6][3]usize{ .{ 0, 1, 2 }, .{ 0, 2, 1 }, .{ 1, 0, 2 }, .{ 1, 2, 0 }, .{ 2, 0, 1 }, .{ 2, 1, 0 } };
    var fits: std.ArrayList(u8) = .empty;
    defer fits.deinit(gpa);
    var best_c: usize = 0;
    var best_perm = [3]usize{ 0, 0, 0 };
    var best_sign = [3]i32{ 1, 1, 1 };
    for (perms) |perm| {
        for (0..8) |bits| {
            const sign = [3]i32{ if (bits & 1 != 0) -1 else 1, if (bits & 2 != 0) -1 else 1, if (bits & 4 != 0) -1 else 1 };
            const c = Fit.inside(vox.items, perm, sign, gmin, gmax);
            if (c > best_c) {
                best_c = c;
                best_perm = perm;
                best_sign = sign;
            }
            if (c == vox.items.len and fits.items.len < 200) fits.print(gpa, " perm={any} sign={any}", .{ perm, sign }) catch {};
        }
    }
    logf("{s}: indices sampled={d} range {d}..{d} (span {d}); vertices={d} raw_pos min={any} max={any}; voxel min={any} max={any} grid max={any}; identity in-grid {d}/{d}; full-fit transforms:{s}; best {d}/{d} perm={any} sign={any}", .{
        head,                                                 n,                                  imin,          imax, span, vox.items.len, raw_min, raw_max, vmin, vmax, gmax,
        Fit.inside(vox.items, .{ 0, 1, 2 }, .{ 1, 1, 1 }, gmin, gmax), vox.items.len, if (fits.items.len == 0) " none" else fits.items,
        best_c,                                               vox.items.len,                      best_perm,     best_sign,
    });
}

fn endBlockDraw(g: *const Gl, saved: SavedState) void {
    g.active_texture(@bitCast(saved.active_texture));
    g.use_program(@bitCast(saved.program));
}

/// Build (or reuse) the FLIR opaque-program copy for the game program `game`.
/// Attribute locations are per program, so the program id is the cache key and
/// a hit costs no GL queries. A failed build is cached as null: never retry it
/// (and its info log) on every draw.
fn blockProgram(g: *const Gl, game: u32) ?BlockProgram {
    {
        const programs = BLOCK_PROGRAMS.lock();
        defer BLOCK_PROGRAMS.unlock();
        for (programs.items) |e| if (e.game_program == game) return e.program;
        // Every entry owns a linked GL program: past the cap, leave further game
        // programs to the game instead of linking one per draw (the old cap of 8
        // without caching re-linked on every draw of a ninth program).
        if (programs.items.len >= 32) return null;
    }
    glq(@src());
    const pos_loc = g.get_attrib_location(game, "vertex_position_in");
    glq(@src());
    const norm_loc = g.get_attrib_location(game, "vertex_normal_in");
    glq(@src());
    const color_loc = g.get_attrib_location(game, "vertex_color_in");
    const built: ?BlockProgram = if (pos_loc < 0 or norm_loc < 0) blk: {
        logOnce(LOG_FLIR_NO_ATTRIB, "FLIR: game program {d} lacks vertex_position_in/vertex_normal_in; block pass disabled for it", .{game});
        break :blk null;
    } else buildBlockProgram(g, pos_loc, norm_loc, color_loc);
    const programs = BLOCK_PROGRAMS.lock();
    defer BLOCK_PROGRAMS.unlock();
    programs.append(gpa, .{ .game_program = game, .program = built }) catch {};
    return built;
}

const BlockAttribBinder = struct {
    pos: i32,
    norm: i32,
    color: i32,
    fn bind(self: BlockAttribBinder, g: *const Gl, program: u32) void {
        g.bind_attrib_location(program, @bitCast(self.pos), "vertex_position_in");
        g.bind_attrib_location(program, @bitCast(self.norm), "vertex_normal_in");
        if (self.color >= 0) g.bind_attrib_location(program, @bitCast(self.color), "vertex_color_in");
        g.bind_frag_data_location(program, 0, "flir_out0");
        g.bind_frag_data_location(program, 1, "flir_out1");
        g.bind_frag_data_location(program, 2, "flir_out2");
    }
};

fn buildBlockProgram(g: *const Gl, pos_loc: i32, norm_loc: i32, color_loc: i32) ?BlockProgram {
    const program = linkProgram(g, BLOCK_VS, BLOCK_FS, BlockAttribBinder{ .pos = pos_loc, .norm = norm_loc, .color = color_loc }, BlockAttribBinder.bind) orelse return null;
    const block: BlockProgram = .{
        .program = program,
        .mat_view_proj = uniform(g, program, "mat_view_proj"),
        .mat_world = uniform(g, program, "mat_world"),
        .comp_to_body = uniform(g, program, "comp_to_body"),
        .grid_min = uniform(g, program, "grid_min"),
        .grid_dims = uniform(g, program, "grid_dims"),
        .enc_min = uniform(g, program, "enc_min"),
        .enc_max = uniform(g, program, "enc_max"),
        .tex_temps = uniform(g, program, "tex_temps"),
        .normal_encoded = uniform(g, program, "normal_encoded"),
        .comp_voxel = uniform(g, program, "comp_voxel"),
        .has_color = uniform(g, program, "has_color"),
        .color_loc = color_loc,
        .has_comp_voxel = uniform(g, program, "has_comp_voxel"),
    };
    if (block.mat_view_proj < 0 or block.mat_world < 0 or block.comp_to_body < 0 or block.grid_min < 0 or
        block.grid_dims < 0 or block.enc_min < 0 or block.enc_max < 0 or block.tex_temps < 0 or block.normal_encoded < 0)
    {
        logOnce(LOG_FLIR_SHADER, "FLIR: block program is missing a required uniform; block pass disabled", .{});
        g.delete_program(program);
        return null;
    }
    logDebug("FLIR: block program linked (program {d}, attrib locations ({d}, {d}))", .{ program, pos_loc, norm_loc });
    return block;
}

/// Save, zero and restore the unpack state around an upload. A non-default
/// `GL_UNPACK_ROW_LENGTH`/`SKIP_*`/`IMAGE_HEIGHT` or a bound
/// `GL_PIXEL_UNPACK_BUFFER` makes the driver read outside our staging slice
/// (a native fault no vectored handler can catch under Proton). Saved with the
/// client attribute stack and the tracked PBO binding: the old 7 glGetIntegerv
/// per upload were the bulk of the plugin's driver syncs.
const UnpackGuard = struct {
    g: *const Gl,
    pbo: i32,

    fn begin(g: *const Gl) UnpackGuard {
        g.push_client_attrib(GL_CLIENT_PIXEL_STORE_BIT);
        const pbo = tracked(g, &TRACK_UNPACK_PBO, GL_PIXEL_UNPACK_BUFFER_BINDING);
        if (pbo != 0) g.bind_buffer(GL_PIXEL_UNPACK_BUFFER, 0);
        g.pixel_storei(GL_UNPACK_ALIGNMENT, 1);
        g.pixel_storei(GL_UNPACK_ROW_LENGTH, 0);
        g.pixel_storei(GL_UNPACK_IMAGE_HEIGHT, 0);
        g.pixel_storei(GL_UNPACK_SKIP_ROWS, 0);
        g.pixel_storei(GL_UNPACK_SKIP_PIXELS, 0);
        g.pixel_storei(GL_UNPACK_SKIP_IMAGES, 0);
        return .{ .g = g, .pbo = pbo };
    }

    fn end(self: UnpackGuard) void {
        self.g.pop_client_attrib();
        if (self.pbo != 0) self.g.bind_buffer(GL_PIXEL_UNPACK_BUFFER, @bitCast(self.pbo));
    }
};

/// Bind this body's R32F volume, uploading the grid when its version changed
/// (throttled to one upload per body per `FLIR_UPLOAD_INTERVAL_FRAMES`, so a
/// body that warms up continuously does not re-upload every frame). The returned
/// entry describes the texture that is resident (`min`/`dims`), so the shader's
/// texel math can never disagree with the uploaded data.
fn bodyTexture(g: *const Gl, body: usize) ?GridTexture {
    const frame = FRAME.load(.monotonic);
    const cached = blk: {
        const textures = GRID_TEXTURES.lock();
        defer GRID_TEXTURES.unlock();
        break :blk textures.get(body);
    };
    if (cached) |entry| {
        if (frame < entry.frame +| offsets.FLIR_UPLOAD_INTERVAL_FRAMES) return entry;
    }

    // Cheap metadata read: nothing is copied unless the grid actually changed.
    const meta = blk: {
        const published = blocktemp.publishedLock();
        defer blocktemp.publishedUnlock();
        const grid = published.getPtr(body) orelse return null;
        break :blk .{ .version = grid.version, .dims = grid.dims, .min = grid.min };
    };
    const version = meta.version;
    const dims = meta.dims;
    const min = meta.min;
    if (cached) |entry| {
        if (entry.version == version and std.meta.eql(entry.dims, dims)) {
            const textures = GRID_TEXTURES.lock();
            defer GRID_TEXTURES.unlock();
            if (textures.getPtr(body)) |stored| stored.frame = frame;
            var refreshed = entry;
            refreshed.frame = frame;
            return refreshed;
        }
    }

    // Bound the dims BEFORE multiplying: a bogus grid must not overflow. The
    // sim's own grid cap is the single source of truth for the limit.
    for (dims) |d| if (d == 0 or d > blocktemp.MAX_GRID_DIM) return null;
    const expected = @as(usize, dims[0]) * dims[1] * dims[2];
    if (expected == 0) return null;

    // Copy the grid into our own staging buffer while it is borrowed, so the
    // sim's published grid is never cloned and the GL call runs lock-free. The
    // render thread is the buffer's only user, so holding it across the upload
    // cannot block the game thread. Re-checking version/dims inside the same
    // borrow keeps the texture and `min`/`dims` consistent.
    const staging = UPLOAD_BUF.lock();
    defer UPLOAD_BUF.unlock();
    const copied = blk: {
        const published = blocktemp.publishedLock();
        defer blocktemp.publishedUnlock();
        const grid = published.getPtr(body) orelse break :blk false;
        if (grid.version != version or !std.meta.eql(grid.dims, dims) or grid.temps_c.items.len < expected) break :blk false;
        staging.clearRetainingCapacity();
        staging.appendSlice(gpa, grid.temps_c.items[0..expected]) catch break :blk false;
        break :blk true;
    };
    if (!copied) {
        logOnce(LOG_FLIR_GRID_MISMATCH, "FLIR: body 0x{x} grid dims {any} do not match {d} temperatures; block pass skipped", .{ body, dims, expected });
        return null;
    }

    // Drop textures no view has used for two frames (never the one we are about
    // to draw with); the ids go out of the lock and are deleted after unbinding.
    var stale: std.ArrayList(u32) = .empty;
    defer stale.deinit(gpa);
    {
        const textures = GRID_TEXTURES.lock();
        defer GRID_TEXTURES.unlock();
        if (textures.count() > offsets.FLIR_GRID_TEXTURE_MAX) {
            var drop: std.ArrayList(usize) = .empty;
            defer drop.deinit(gpa);
            var it = textures.iterator();
            while (it.next()) |e| {
                if (e.key_ptr.* != body and e.value_ptr.frame +| 2 < frame) drop.append(gpa, e.key_ptr.*) catch {};
            }
            for (drop.items) |key| {
                if (textures.fetchRemove(key)) |kv| stale.append(gpa, kv.value.texture) catch {};
            }
        }
    }
    if (stale.items.len > 0) {
        g.bind_texture(GL_TEXTURE_3D, 0);
        LAST_3D.store(0, .monotonic);
        g.delete_textures(@intCast(stale.items.len), stale.items.ptr);
        logDebug("FLIR: evicted {d} stale body textures", .{stale.items.len});
    }

    const texture: u32 = blk: {
        const textures = GRID_TEXTURES.lock();
        defer GRID_TEXTURES.unlock();
        if (textures.get(body)) |entry| break :blk entry.texture;
        var id: u32 = 0;
        g.gen_textures(1, @ptrCast(&id));
        if (id == 0) return null;
        break :blk id;
    };
    g.bind_texture(GL_TEXTURE_3D, texture);
    LAST_3D.store(texture, .monotonic);
    {
        const unpack = UnpackGuard.begin(g);
        defer unpack.end();
        // Same volume size as last time: overwrite the texels in place instead
        // of reallocating the texture storage for every upload.
        if (cached != null and std.meta.eql(cached.?.dims, dims)) {
            g.tex_sub_image_3d(GL_TEXTURE_3D, 0, 0, 0, 0, @intCast(dims[0]), @intCast(dims[1]), @intCast(dims[2]), GL_RED, GL_FLOAT, staging.items.ptr);
        } else {
            g.tex_parameteri(GL_TEXTURE_3D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            g.tex_parameteri(GL_TEXTURE_3D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            g.tex_parameteri(GL_TEXTURE_3D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            g.tex_parameteri(GL_TEXTURE_3D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
            g.tex_parameteri(GL_TEXTURE_3D, GL_TEXTURE_WRAP_R, GL_CLAMP_TO_EDGE);
            g.tex_image_3d(GL_TEXTURE_3D, 0, @intCast(GL_R32F), @intCast(dims[0]), @intCast(dims[1]), @intCast(dims[2]), 0, GL_RED, GL_FLOAT, staging.items.ptr);
        }
    }
    const entry: GridTexture = .{ .texture = texture, .version = version, .min = min, .dims = dims, .frame = frame };
    const textures = GRID_TEXTURES.lock();
    defer GRID_TEXTURES.unlock();
    textures.put(gpa, body, entry) catch {};
    return entry;
}

/// Describe `fbo` as a G-buffer, or return null when it is not one (no two
/// colour textures). Known framebuffers come from the registry without any GL
/// query, so a pass that binds an unknown target pays one probe and nothing
/// after that.
fn gbufferFor(g: *const Gl, fbo: u32) ?Gbuffer {
    if (fbo == 0) return null;
    if (cachedGbuffer(fbo)) |entry| return entry;
    {
        const rejected = NOT_GBUFFERS.lock();
        defer NOT_GBUFFERS.unlock();
        if (rejected.contains(fbo)) return null;
    }
    if (captureGbuffer(g, fbo)) |entry| return entry;
    const rejected = NOT_GBUFFERS.lock();
    defer NOT_GBUFFERS.unlock();
    if (rejected.count() >= NOT_GBUFFER_LIMIT) rejected.clearRetainingCapacity();
    rejected.put(gpa, fbo, {}) catch {};
    return null;
}

/// Probe `fbo`'s attachments: 0 = `gnormal_light_factor`, 1 = `gcolor`, plus
/// depth (RE: the `glDrawBuffers` list is the fixed COLOR_ATTACHMENT0..4 and the
/// opaque shader declares its outputs in that order). Anything without two
/// colour attachments (tonemap, shadow, bloom) is not a G-buffer.
fn captureGbuffer(g: *const Gl, fbo: u32) ?Gbuffer {
    // Query the attachments of `fbo` (it is the bound draw framebuffer at every
    // call site).
    const normal = attachmentTexture(g, GL_COLOR_ATTACHMENT0) orelse 0;
    const color = attachmentTexture(g, GL_COLOR_ATTACHMENT0 + 1) orelse 0;
    if (normal == 0 or color == 0) return null;
    const depth = attachmentTexture(g, GL_DEPTH_ATTACHMENT) orelse attachmentTexture(g, GL_DEPTH_STENCIL_ATTACHMENT) orelse 0;
    const normal_type = attachmentComponentType(g, GL_COLOR_ATTACHMENT0);
    const color_type = attachmentComponentType(g, GL_COLOR_ATTACHMENT0 + 1);
    // Raw normals in a normalized attachment cannot hold negative components, so
    // the composite then decodes packed xy and moves the vehicle marker to
    // gcolor alpha instead of the light-factor channel.
    const normal_encoded = normal_type != @as(i32, @intCast(GL_FLOAT));
    const normal_format = attachmentInternalFormat(g, normal);
    const color_format = attachmentInternalFormat(g, color);
    const normal_size = textureSize(g, normal);
    const entry: Gbuffer = .{ .fbo = fbo, .tex_normal = normal, .tex_color = color, .depth = depth, .size = normal_size, .normal_encoded = normal_encoded };
    {
        const registry = GBUFFERS.lock();
        defer GBUFFERS.unlock();
        // A long session can cycle resolution targets; keep the registry small
        // rather than letting it grow without bound (a cleared entry is simply
        // probed again on its next bind).
        if (registry.count() >= 8 and !registry.contains(fbo)) registry.clearRetainingCapacity();
        registry.put(gpa, fbo, entry) catch {};
    }
    logOnceKey(0x06a5_8000 | (@as(u64, fbo) & 0x3fff), "FLIR: G-buffer captured from FBO {d} ({d}x{d}): normal(tex {d}, internal format 0x{x}, component type 0x{x}, encoded={}), colour(tex {d}, internal format 0x{x}, component type 0x{x}), depth={d} (0 = renderbuffer attachment)", .{
        fbo, normal_size[0], normal_size[1], normal, normal_format, normal_type, normal_encoded, color, color_format, color_type, depth,
    });
    return entry;
}

/// `GL_TEXTURE_INTERNAL_FORMAT` of an attachment texture (0 when unknown). The
/// storage format decides whether the float light-factor marker and raw normals
/// survive, so it is probed (and logged) once per captured G-buffer.
fn attachmentInternalFormat(g: *const Gl, texture: u32) i32 {
    if (texture == 0) return 0;
    // glGetTexLevelParameteriv reads the texture bound to GL_TEXTURE_2D on the
    // active unit, so bind ours and put the game's binding back.
    var previous: i32 = 0;
    glq(@src());
    g.get_integerv(GL_TEXTURE_BINDING_2D, @ptrCast(&previous));
    g.bind_texture(GL_TEXTURE_2D, texture);
    var format: i32 = 0;
    glq(@src());
    g.get_tex_level_parameteriv(GL_TEXTURE_2D, 0, GL_TEXTURE_INTERNAL_FORMAT, &format);
    g.bind_texture(GL_TEXTURE_2D, @bitCast(previous));
    return format;
}

/// Texel size of an attachment texture (`[0, 0]` when unknown). Diagnostics.
fn textureSize(g: *const Gl, texture: u32) [2]i32 {
    if (texture == 0) return .{ 0, 0 };
    var previous: i32 = 0;
    glq(@src());
    g.get_integerv(GL_TEXTURE_BINDING_2D, @ptrCast(&previous));
    g.bind_texture(GL_TEXTURE_2D, texture);
    var width: i32 = 0;
    var height: i32 = 0;
    glq(@src());
    g.get_tex_level_parameteriv(GL_TEXTURE_2D, 0, GL_TEXTURE_WIDTH, &width);
    glq(@src());
    g.get_tex_level_parameteriv(GL_TEXTURE_2D, 0, GL_TEXTURE_HEIGHT, &height);
    g.bind_texture(GL_TEXTURE_2D, @bitCast(previous));
    return .{ width, height };
}

/// `GL_FRAMEBUFFER_ATTACHMENT_COMPONENT_TYPE` of an attachment (0 when unbound).
fn attachmentComponentType(g: *const Gl, attachment: u32) i32 {
    var kind: i32 = GL_NONE;
    glq(@src());
    g.get_framebuffer_attachment_parameteriv(GL_FRAMEBUFFER, attachment, GL_FRAMEBUFFER_ATTACHMENT_COMPONENT_TYPE, &kind);
    return kind;
}

fn attachmentTexture(g: *const Gl, attachment: u32) ?u32 {
    var kind: i32 = GL_NONE;
    glq(@src());
    g.get_framebuffer_attachment_parameteriv(GL_FRAMEBUFFER, attachment, GL_FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE, &kind);
    if (kind != @as(i32, @intCast(GL_TEXTURE))) return null;
    var id: i32 = 0;
    glq(@src());
    g.get_framebuffer_attachment_parameteriv(GL_FRAMEBUFFER, attachment, GL_FRAMEBUFFER_ATTACHMENT_OBJECT_NAME, &id);
    return if (id > 0) @intCast(id) else null;
}

// ---------------------------------------------------------------------------
// Composite pass
// ---------------------------------------------------------------------------

/// GL state the composite changes, restored by `CompositeGuard.end`: enables,
/// colour/depth masks and units 0-2 texture bindings (plus the active unit) go
/// through the compat attribute stack, the program through the tracker - no
/// driver query. The composite never binds a VAO (it draws the game's quad).
const CompositeGuard = struct {
    g: *const Gl,
    program: i32,

    fn begin(g: *const Gl) CompositeGuard {
        const program = tracked(g, &TRACK_PROGRAM, GL_CURRENT_PROGRAM);
        g.push_attrib(COMPOSITE_ATTRIB_BITS);
        g.disable(GL_DEPTH_TEST);
        g.disable(GL_BLEND);
        g.disable(GL_CULL_FACE);
        g.color_mask(1, 1, 1, 1);
        g.depth_mask(0);
        return .{ .g = g, .program = program };
    }

    fn end(self: CompositeGuard) void {
        self.g.pop_attrib();
        self.g.use_program(@bitCast(self.program));
    }
};

const COMPOSITE_ATTRIB_BITS: u32 = GL_ENABLE_BIT | GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT | GL_TEXTURE_BIT;

/// Viewport size last seen per tonemap OUTPUT target. Keyed by the target, not
/// by the tonemap-pass ordinal: one frame renders the main view and a camera
/// feed at different sizes, which is not a resolution change.
const VIEW_SIZE_TARGETS: usize = 8;
var VIEW_SIZE: Mutex(fx.HashMap(u32, [2]i32)) = .{ .value = .empty };

/// True when this target's size differs from the last time it was composited.
/// The target-keyed caches are then dropped: after a window or resolution change
/// the game deletes and recreates its render targets and the driver reuses the
/// names, so a cached entry would describe a target that no longer exists - or a
/// recycled id would stay on the rejected list and keep FLIR dead.
/// `MESH_BODY` is deliberately NOT cleared here: it is keyed by mesh index-buffer
/// ids (which a view resize does not touch) and is rebuilt on the game thread
/// every 250 ms, so clearing it from the render thread can only empty a freshly
/// published map.
fn forgetCachesOnResize(target: u32, width: i32, height: i32) bool {
    const previous: [2]i32 = blk: {
        const sizes = VIEW_SIZE.lock();
        defer VIEW_SIZE.unlock();
        if (sizes.count() >= VIEW_SIZE_TARGETS and !sizes.contains(target)) sizes.clearRetainingCapacity();
        const old = (sizes.fetchPut(gpa, target, .{ width, height }) catch return false) orelse return false;
        break :blk old.value;
    };
    if ((previous[0] == width and previous[1] == height) or (previous[0] == 0 and previous[1] == 0)) return false;
    {
        const g = GBUFFERS.lock();
        defer GBUFFERS.unlock();
        g.clearRetainingCapacity();
    }
    {
        const n = NOT_GBUFFERS.lock();
        defer NOT_GBUFFERS.unlock();
        n.clearRetainingCapacity();
    }
    // The target/G-buffer ids are recycled as well, so every target fact and
    // pairing is dropped: a G-buffer is only paired again at a tonemap quad.
    {
        const targets = TARGETS.lock();
        defer TARGETS.unlock();
        targets.by_fbo.clearRetainingCapacity();
        targets.gbuf_target.clearRetainingCapacity();
        targets.target_gbuf.clearRetainingCapacity();
    }
    refreshForcedGbuffers();
    logDebug("FLIR: tonemap target {d} is now {d}x{d} (was {d}x{d}); G-buffer, target-pairing and rejected-FBO caches dropped (the mesh->body map is kept)", .{ target, width, height, previous[0], previous[1] });
    return true;
}

const QuadLocs = struct { pos: i32, coord: i32, mvp: i32 };
const QuadLocsEntry = struct { program: u32, locs: QuadLocs };
var QUAD_LOCS: Mutex(std.ArrayList(QuadLocsEntry)) = .{ .value = .empty };

/// The tonemap program's quad inputs (position, coord0, mat_view_proj), cached
/// per program: attribute/uniform locations never change after linking.
fn quadLocs(g: *const Gl, program: u32) QuadLocs {
    {
        const locs = QUAD_LOCS.lock();
        defer QUAD_LOCS.unlock();
        for (locs.items) |e| if (e.program == program) return e.locs;
    }
    glq(@src());
    const pos = g.get_attrib_location(program, "vertex_position_in");
    glq(@src());
    const coord = g.get_attrib_location(program, "vertex_coord0_in");
    glq(@src());
    const mvp = g.get_uniform_location(program, "mat_view_proj");
    const l: QuadLocs = .{ .pos = pos, .coord = coord, .mvp = mvp };
    const locs = QUAD_LOCS.lock();
    defer QUAD_LOCS.unlock();
    if (locs.items.len >= 64) locs.clearRetainingCapacity();
    locs.append(gpa, .{ .program = program, .locs = l }) catch {};
    return l;
}

const ReadbackKey = struct { target: u32, gbuffer: u32 };
var READBACK_DONE: Mutex(std.ArrayList(ReadbackKey)) = .{ .value = .empty };

/// Draw the FLIR composite where the tonemap quad would have drawn: into the
/// tonemap output target the quad is drawing into, over the whole current
/// viewport, so the FXAA pass that follows presents it. Returns whether the draw
/// happened; a `false` lets the game's own tonemap quad run.
///
/// `ir_graded` = this target's tonemap quad was IR-graded (its own `0x68B8C0`
/// params); `forced` = debug F10 on the main view; `main` = the target was
/// learned as the main view (diagnostics/F10 only); `camera` = the IR-latched
/// camera an IR view takes its config/ambient from (0 for the main view). The
/// composite samples ONLY the G-buffer this target was paired with at its
/// tonemap quad - never "the last G-buffer seen", which belongs to whichever
/// view drew before.
fn compositeView(target: u32, ir_graded: bool, forced: bool, main: bool, camera: usize, quad: Quad) bool {
    fault.enter(stage.FLIR_COMPOSITE);
    defer fault.leave();
    const maybe_gl = gl();
    const viewport: [4]i32 = if (maybe_gl) |g| currentViewport(g) else .{ 0, 0, 0, 0 };
    // Key of a skipped composite: reason, view size and target.
    const skipKey = struct {
        fn of(reason: u64, vp: [4]i32, t: u32) u64 {
            return (reason << 56) | (@as(u64, @as(u32, @bitCast(vp[2]))) << 32) | (@as(u64, @as(u32, @bitCast(vp[3]))) << 16) | t;
        }
    }.of;
    if (COMPOSITE_FAILED.load(.monotonic)) {
        logCompositeSkip(forced, skipKey(1, viewport, target), "the composite program could not be built", .{});
        return false;
    }
    const g = maybe_gl orelse {
        logCompositeSkip(forced, skipKey(7, viewport, target), "the GL entry points are unavailable", .{});
        return false;
    };
    if (viewport[2] <= 0 or viewport[3] <= 0) {
        logCompositeSkip(forced, skipKey(2, viewport, target), "the viewport is empty ({d}x{d})", .{ viewport[2], viewport[3] });
        return false;
    }
    logViewMode(viewport, target, ir_graded, forced, main);
    // Resize detection is per output target, with the size read at the quad draw.
    if (forgetCachesOnResize(target, viewport[2], viewport[3])) {
        // The caches were dropped under this view's feet: keep the game's image
        // for this frame, the next one re-pairs and composites.
        logCompositeSkip(forced, skipKey(3, viewport, target), "the tonemap target {d} changed size to {d}x{d} (caches dropped, retrying next frame)", .{ target, viewport[2], viewport[3] });
        return false;
    }
    // Only the G-buffer this target was paired with (learned at this or a
    // previous tonemap quad) may be sampled.
    const gbuffer = pairedGbuffer(target) orelse {
        if (verbose) {
            var known = knownTargets();
            defer known.deinit(gpa);
            logCompositeSkip(forced, skipKey(4, viewport, target), "no usable G-buffer is paired with tonemap target {d} yet (the pairing is learned at the tonemap quad; targets seen so far: {any}); the game's tonemap is kept", .{ target, known.items });
        }
        return false;
    };
    // The game's tonemap program is still bound: take its quad attribute
    // locations and view-projection so our program draws the same geometry.
    const game_program = tracked(g, &TRACK_PROGRAM, GL_CURRENT_PROGRAM);
    const locs: QuadLocs = if (game_program > 0) quadLocs(g, @intCast(game_program)) else .{ .pos = -1, .coord = -1, .mvp = -1 };
    if (locs.pos < 0 or locs.coord < 0 or locs.mvp < 0) {
        logCompositeSkip(forced, skipKey(8, viewport, target), "the game's tonemap program {d} lacks the quad inputs (position {d}, coord0 {d}, mat_view_proj {d}); the game's tonemap is kept", .{ game_program, locs.pos, locs.coord, locs.mvp });
        return false;
    }
    // The matrix hook keeps each program's last mat_view_proj upload (raw,
    // with its transpose flag); only query when it has not seen one.
    const recorded: ?Mat = blk: {
        const mats = PROG_MATS.lock();
        defer PROG_MATS.unlock();
        break :blk if (mats.get(@intCast(game_program))) |m| m.vp else null;
    };
    var mat_view_proj = [_]f32{0} ** 16;
    if (recorded) |r| {
        mat_view_proj = toColmajor(&r.m, r.transpose);
    } else {
        glq(@src());
        g.get_uniformfv(@intCast(game_program), locs.mvp, &mat_view_proj);
    }
    const comp = composite(g, @intCast(locs.pos), @intCast(locs.coord)) orelse {
        logCompositeSkip(forced, skipKey(6, viewport, target), "the composite program is unavailable", .{});
        return false;
    };

    // Config comes from the camera the IR view's tonemap quad ran with (the
    // newest IR-latched camera); the main view (F10) has no camera. Ambient is
    // that camera's pivot temperature, else the scene median cached on the game
    // thread (see `sceneTick`).
    const config: state.FlirCfg = if (camera >= PTR_MIN) cameraConfig(camera) else .{};
    const sun_t = sunToward();
    const ambient = if (camera >= PTR_MIN) cameraAmbient(camera) else sceneAmbient();

    const guard = CompositeGuard.begin(g);
    g.use_program(comp.program);
    g.uniform_matrix4fv(comp.mat_view_proj, 1, 0, &mat_view_proj);
    g.active_texture(GL_TEXTURE0);
    g.bind_texture(GL_TEXTURE_2D, gbuffer.tex_normal);
    g.uniform1i(comp.tex_normal, 0);
    g.active_texture(GL_TEXTURE0 + 1);
    g.bind_texture(GL_TEXTURE_2D, gbuffer.tex_color);
    g.uniform1i(comp.tex_color, 1);
    g.active_texture(GL_TEXTURE0 + 2);
    g.bind_texture(GL_TEXTURE_2D, gbuffer.depth);
    g.uniform1i(comp.tex_depth, 2);
    g.uniform1i(comp.has_depth, @intFromBool(gbuffer.depth != 0));
    g.uniform1i(comp.normal_encoded, @intFromBool(gbuffer.normal_encoded));
    g.uniform3f(comp.sun_dir, sun_t.dir[0], sun_t.dir[1], sun_t.dir[2]);
    g.uniform1f(comp.sun_valid, sun_t.valid);
    g.uniform1f(comp.day_pct, @bitCast(DAY_PCT_BITS.load(.monotonic)));
    g.uniform1f(comp.clear_sky, @bitCast(CLEAR_SKY_BITS.load(.monotonic)));
    const inv = blk: {
        const m = LIGHT_INV.lock();
        defer LIGHT_INV.unlock();
        break :blk m.*;
    };
    if (inv) |m| {
        g.uniform_matrix4fv(comp.mat_inv, 1, 0, &m);
        g.uniform1i(comp.has_inv, 1);
    } else {
        g.uniform1i(comp.has_inv, 0);
    }
    // In game at altitude: graphics_offset.y = -2000 with the eye at render y
    // -278, i.e. world = render - graphics_offset, so the sea (world y 0) is
    // render y = graphics_offset.y.
    g.uniform1f(comp.water_y, @bitCast(LIGHT_OFFSET_Y_BITS.load(.monotonic)));
    const ground: f32 = @bitCast(GROUND_AMBIENT_BITS.load(.monotonic));
    g.uniform1f(comp.ambient_c, if (std.math.isFinite(ground)) ground else ambient);
    g.uniform1f(comp.vehicle_air_c, ambient);
    const water_tex = LIGHT_WATER_TEX.load(.monotonic);
    g.active_texture(GL_TEXTURE0 + 3);
    g.bind_texture(GL_TEXTURE_2D, water_tex);
    g.uniform1i(comp.tex_water, 3);
    g.uniform1i(comp.has_water, @intFromBool(water_tex != 0));
    const plumes = viewPlumes();
    g.uniform1i(comp.plume_n, @intCast(plumes.n));
    if (plumes.n > 0) {
        g.uniform4fv(comp.plume_a, @intCast(plumes.n), @ptrCast(&plumes.a));
        g.uniform4fv(comp.plume_b, @intCast(plumes.n), @ptrCast(&plumes.b));
        logOnceKey(0x6a5_0010, "FLIR plume: first drawn {any} {any}", .{ plumes.a[0], plumes.b[0] });
    }
    g.uniform1f(comp.sun_soak_c, offsets.FLIR_SUN_SOAK_C);
    g.uniform3f(comp.tint, config.rgb[0], config.rgb[1], config.rgb[2]);
    g.uniform1i(comp.black_hot, @intFromBool(config.black_hot));
    g.uniform1f(comp.tmin, config.tmin);
    g.uniform1f(comp.tmax, config.tmax);
    g.uniform1f(comp.enc_min, offsets.FLIR_ENC_MIN_C);
    g.uniform1f(comp.enc_max, offsets.FLIR_ENC_MAX_C);
    g.uniform1f(comp.sky_c, offsets.FLIR_SKY_C);
    g.uniform1f(comp.sky_depth_m, offsets.FLIR_SKY_DEPTH_M);
    // The game's own VAO/element buffer stay bound: same quad, same indices.
    g.draw_elements(quad.mode, quad.count, quad.kind, quad.indices);
    if (verbose) {
        logQuadInputs(g, main, locs, &mat_view_proj, quad);
        // One-shot proof of what the composite wrote (centre pixel, RGBA).
        // Once per (target, G-buffer) pair: a world load re-pairs the main view.
        const first = blk: {
            const done = READBACK_DONE.lock();
            defer READBACK_DONE.unlock();
            const key: ReadbackKey = .{ .target = target, .gbuffer = gbuffer.fbo };
            for (done.items) |k| if (std.meta.eql(k, key)) break :blk false;
            if (done.items.len >= 32) break :blk false;
            done.append(gpa, key) catch {};
            break :blk true;
        };
        if (first) {
            var draw: i32 = 0;
            var read: i32 = 0;
            glq(@src());
            g.get_integerv(GL_FRAMEBUFFER_BINDING, @ptrCast(&draw));
            glq(@src());
            g.get_integerv(GL_READ_FRAMEBUFFER_BINDING, @ptrCast(&read));
            g.bind_framebuffer(GL_READ_FRAMEBUFFER, @bitCast(draw));
            var px = [4]f32{ std.math.nan(f32), std.math.nan(f32), std.math.nan(f32), std.math.nan(f32) };
            const cx = viewport[0] + @divTrunc(viewport[2], 2);
            const cy = viewport[1] + @divTrunc(viewport[3], 2);
            g.read_pixels(cx, cy, 1, 1, GL_RGBA, GL_FLOAT, &px);
            g.bind_framebuffer(GL_READ_FRAMEBUFFER, @bitCast(read));
            logf("FLIR: composite readback target {d} (FBO {d}, {s} view) centre ({d},{d}) RGBA={any} ambient={d:.1} window=[{d}, {d}]", .{
                target, draw, if (main) "main" else "feed/other", cx, cy, px, ambient, config.tmin, config.tmax,
            });
        }
    }
    guard.end();

    const pair_key = 0x06a5_4000 | ((@as(u64, gbuffer.fbo) & 0x7f) << 7) | (@as(u64, target) & 0x7f);
    logOnceKey(pair_key, "FLIR: composite drawn {d}x{d} into tonemap target {d} ({s} view, IR={d} F10={d}): tint={any} black_hot={} window=[{d}, {d}] C ambient={d:.1} C gbuffer=(fbo {d}, {d}x{d}, normal {d}, colour {d}, depth {d})", .{
        viewport[2],            viewport[3],        target,                   if (main) "main" else "feed/other", @intFromBool(ir_graded), @intFromBool(forced),
        config.rgb,             config.black_hot,   config.tmin,              config.tmax,                        ambient,                 gbuffer.fbo,
        gbuffer.size[0],        gbuffer.size[1],    gbuffer.tex_normal,       gbuffer.tex_color,                  gbuffer.depth,
    });
    return true;
}

fn asyncKeyDown(vk: i32) ?bool {
    if (!GET_KEY_STATE_RESOLVED.load(.acquire)) {
        const module = win.LoadLibraryA("user32.dll");
        const address: usize = if (module != null) (if (win.GetProcAddress(module, "GetAsyncKeyState")) |p| @intFromPtr(p) else 0) else 0;
        GET_KEY_STATE.store(address, .monotonic);
        GET_KEY_STATE_RESOLVED.store(true, .release);
    }
    const address = GET_KEY_STATE.load(.monotonic);
    if (address == 0) return null;
    const f: *const fn (i32) callconv(API) i16 = @ptrFromInt(address);
    return @as(u16, @bitCast(f(vk))) & 0x8000 != 0;
}

var QUAD_LOGGED_MAIN: Atomic(bool) = .init(false);
var QUAD_LOGGED_FEED: Atomic(bool) = .init(false);

/// One-shot (per main / feed view) log of the game's quad inputs: attribute
/// locations, `mat_view_proj` and the min/max of the texcoords the quad feeds.
fn logQuadInputs(g: *const Gl, main: bool, locs: QuadLocs, mvp: *const [16]f32, quad: Quad) void {
    if ((if (main) &QUAD_LOGGED_MAIN else &QUAD_LOGGED_FEED).swap(true, .monotonic)) return;
    const coords = quadCoordRange(g, locs, quad);
    var range_buf: [128]u8 = undefined;
    const range_text = if (coords) |c| std.fmt.bufPrint(&range_buf, "{any}/{any}", .{ c[0], c[1] }) catch "?" else "unreadable";
    logf("FLIR: game quad ({s} view): draw mode=0x{x} count={d} type=0x{x}; attrib vertex_position_in={d} vertex_coord0_in={d} mat_view_proj loc={d} = {any}; coord0 min/max={s}", .{
        if (main) "main" else "feed/other", quad.mode, quad.count, quad.kind, locs.pos, locs.coord, locs.mvp, mvp.*, range_text,
    });
}

fn quadCoordRange(g: *const Gl, locs: QuadLocs, quad: Quad) ?[2][2]f32 {
    const loc: u32 = @intCast(locs.coord);
    var vbo: i32 = 0;
    var size: i32 = 0;
    var ty: i32 = 0;
    var stride: i32 = 0;
    glq(@src());
    g.get_vertex_attribiv(loc, 0x889f, &vbo);
    glq(@src());
    g.get_vertex_attribiv(loc, 0x8623, &size);
    glq(@src());
    g.get_vertex_attribiv(loc, 0x8625, &ty);
    glq(@src());
    g.get_vertex_attribiv(loc, 0x8624, &stride);
    var ptr: ?*const anyopaque = null;
    glq(@src());
    g.get_vertex_attrib_pointerv(loc, 0x8645, &ptr);
    var ibo: i32 = 0;
    glq(@src());
    g.get_integerv(GL_ELEMENT_ARRAY_BUFFER_BINDING, @ptrCast(&ibo));
    const width: usize = switch (quad.kind) {
        0x1401 => 1,
        0x1403 => 2,
        0x1405 => 4,
        else => return null,
    };
    if (vbo <= 0 or @as(u32, @bitCast(ty)) != GL_FLOAT or size < 2 or ibo <= 0 or quad.count < 1 or quad.count > 64) return null;
    const n: usize = @intCast(quad.count);
    var ebo_size: i32 = 0;
    glq(@src());
    g.get_buffer_parameteriv(ELEMENT_ARRAY_BUFFER, BUFFER_SIZE, &ebo_size);
    const first = @intFromPtr(quad.indices);
    if (first + n * width > @as(usize, @intCast(@max(ebo_size, 0)))) return null;
    var raw: [64 * 4]u8 = undefined;
    glq(@src());
    g.get_buffer_sub_data(ELEMENT_ARRAY_BUFFER, @intCast(first), @intCast(n * width), &raw);
    const step: usize = if (stride > 0) @intCast(stride) else @as(usize, @intCast(size)) * 4;
    var prev: i32 = 0;
    var bsize: i32 = 0;
    glq(@src());
    g.get_integerv(ARRAY_BUFFER_BINDING, @ptrCast(&prev));
    g.bind_buffer(ARRAY_BUFFER, @intCast(vbo));
    glq(@src());
    g.get_buffer_parameteriv(ARRAY_BUFFER, BUFFER_SIZE, &bsize);
    var lo = [2]f32{ std.math.floatMax(f32), std.math.floatMax(f32) };
    var hi = [2]f32{ -std.math.floatMax(f32), -std.math.floatMax(f32) };
    var any = false;
    for (0..n) |k| {
        var i: usize = 0;
        var b: usize = width;
        while (b > 0) {
            b -= 1;
            i = (i << 8) | raw[k * width + b];
        }
        const o = @intFromPtr(ptr) + i * step;
        if (o + 8 > @as(usize, @intCast(@max(bsize, 0)))) continue;
        var v = [2]f32{ 0, 0 };
        glq(@src());
        g.get_buffer_sub_data(ARRAY_BUFFER, @intCast(o), 8, &v);
        for (0..2) |a| {
            lo[a] = util.fmin(lo[a], v[a]);
            hi[a] = util.fmax(hi[a], v[a]);
        }
        any = true;
    }
    g.bind_buffer(ARRAY_BUFFER, @bitCast(prev));
    return if (any) .{ lo, hi } else null;
}

/// One line per distinct reason a composite was not drawn, so a missing FLIR
/// image always names its cause. F10-forced attempts are labelled separately:
/// they have no IR grading signature to point at.
fn logCompositeSkip(forced: bool, key: u64, comptime fmt: []const u8, args: anytype) void {
    if (!verbose) return;
    // Bit 7 separates the F10-forced reasons from the flag-driven ones.
    // Exempt 0x6a5 range (a missing composite must always name its cause):
    // reason (top byte of `key`), forced flag, low bits of the target.
    const k = 0x06a5_1000 | (((key >> 56) & 0xf) << 8) | (@as(u64, @intFromBool(forced)) << 7) | (key & 0x7f);
    if (forced) {
        logOnceKey(k, "FLIR: F10 main-view composite skipped: " ++ fmt, args);
    } else {
        logOnceKey(k, "FLIR: composite skipped: " ++ fmt, args);
    }
}

fn cameraConfig(camera: usize) state.FlirCfg {
    const configs = state.FLIR_CONFIG.lock();
    defer state.FLIR_CONFIG.unlock();
    return configs.get(camera) orelse .{};
}

var SUN_LOG_NEXT: Atomic(u64) = .init(0);

fn sunToward() struct { dir: [3]f32, valid: f32 } {
    const calc = blk: {
        const s = GAME_SUN.lock();
        defer GAME_SUN.unlock();
        break :blk s.*;
    };
    const light = blk: {
        const s = LIGHT_SUN.lock();
        defer LIGHT_SUN.unlock();
        break :blk s.*;
    };
    if (light) |l| {
        const v = [3]f32{ -l[0], -l[1], -l[2] };
        if (verbose) logSunCompare(v, calc);
        return .{ .dir = v, .valid = if (v[1] > 0.0) 1.0 else 0.0 };
    }
    if (calc) |v| return .{ .dir = v, .valid = if (v[1] > 0.0) 1.0 else 0.0 };
    if (!state.LIGHT_DIR_VALID.load(.monotonic)) return .{ .dir = .{ 0, 1, 0 }, .valid = 0 };
    const dir = blk: {
        const d = state.LIGHT_DIR.lock();
        defer state.LIGHT_DIR.unlock();
        break :blk d.*;
    };
    // The game's sun_light_direction points from the sun into the scene; the
    // shader soaks surfaces with -dot(dir, normal), so flip it here.
    const v = [3]f32{ @floatCast(-dir[0]), @floatCast(-dir[1]), @floatCast(-dir[2]) };
    const n = @sqrt(v[0] * v[0] + v[1] * v[1] + v[2] * v[2]);
    if (!(n >= 1.0e-4)) return .{ .dir = .{ 0, 1, 0 }, .valid = 0 };
    return .{ .dir = .{ v[0] / n, v[1] / n, v[2] / n }, .valid = if (v[1] > 0.0) 1.0 else 0.0 };
}

fn logSunCompare(v: [3]f32, calc: ?[3]f32) void {
    const now = state.nowMs();
    if (now < SUN_LOG_NEXT.load(.monotonic)) return;
    SUN_LOG_NEXT.store(now + 5000, .monotonic);
    const angles = struct {
        fn el(d: [3]f32) f64 {
            return util.degrees(std.math.asin(@as(f64, std.math.clamp(d[1], -1.0, 1.0))));
        }
        fn az(d: [3]f32) f64 {
            return util.degrees(std.math.atan2(@as(f64, d[2]), @as(f64, d[0])));
        }
    };
    var calc_buf: [160]u8 = undefined;
    const calc_text = if (calc) |c| std.fmt.bufPrint(&calc_buf, "Some({any}) el Some({d}) az Some({d}) | angle Some({d})", .{
        c, angles.el(c), angles.az(c), util.degrees(std.math.acos(@as(f64, std.math.clamp(c[0] * v[0] + c[1] * v[1] + c[2] * v[2], -1.0, 1.0)))),
    }) catch "?" else "None el None az None | angle None";
    // Eye = inverse view-projection of the near-plane centre.
    const inv = blk: {
        const m = LIGHT_INV.lock();
        defer LIGHT_INV.unlock();
        break :blk m.*;
    };
    const eye_y: ?f32 = if (inv) |m| (-m[9] + m[13]) / (-m[11] + m[15]) else null;
    const offset_y: f32 = @bitCast(LIGHT_OFFSET_Y_BITS.load(.monotonic));
    logf("FLIR sun: light-pass to_sun ({f},{f},{f}) el {f} az {f} | calc {s} deg | eye_y(render) {?d} graphics_offset.y {d:.1}", .{
        util.signed(4, v[0]), util.signed(4, v[1]), util.signed(4, v[2]), util.signed(2, angles.el(v)), util.signed(2, angles.az(v)), calc_text, eye_y, offset_y,
    });
}

const CompositeAttribBinder = struct {
    pos: u32,
    coord: u32,
    fn bind(self: CompositeAttribBinder, g: *const Gl, program: u32) void {
        g.bind_attrib_location(program, self.pos, "vertex_position_in");
        g.bind_attrib_location(program, self.coord, "vertex_coord0_in");
    }
};

/// Build (once) the composite program. A failed build disables the composite for
/// the session (fail closed) instead of retrying every frame.
fn composite(g: *const Gl, pos_loc: u32, coord_loc: u32) ?Composite {
    const slot = COMPOSITE.lock();
    defer COMPOSITE.unlock();
    if (slot.*) |c| {
        if (c.pos_loc == pos_loc and c.coord_loc == coord_loc) return c;
        // The game's quad inputs moved: relink with the new locations.
        g.delete_program(c.program);
        slot.* = null;
    }
    if (COMPOSITE_FAILED.load(.monotonic)) return null;
    const program = linkProgram(g, COMPOSITE_VS, COMPOSITE_FS, CompositeAttribBinder{ .pos = pos_loc, .coord = coord_loc }, CompositeAttribBinder.bind) orelse {
        COMPOSITE_FAILED.store(true, .monotonic);
        return null;
    };
    var entry: Composite = undefined;
    entry.program = program;
    entry.pos_loc = pos_loc;
    entry.coord_loc = coord_loc;
    inline for (@typeInfo(Composite).@"struct".fields) |field| {
        if (field.type == i32) @field(entry, field.name) = uniform(g, program, field.name ++ "");
    }
    slot.* = entry;
    return entry;
}

// ---------------------------------------------------------------------------
// Shader helpers
// ---------------------------------------------------------------------------

fn uniform(g: *const Gl, program: u32, name: [:0]const u8) i32 {
    glq(@src());
    return g.get_uniform_location(program, name);
}

/// Compile + link a program; `before_link(ctx, gl, program)` binds attribute
/// and fragment output locations between attach and link.
fn linkProgram(g: *const Gl, vertex_source: [:0]const u8, fragment_source: [:0]const u8, ctx: anytype, comptime before_link: fn (@TypeOf(ctx), *const Gl, u32) void) ?u32 {
    const vertex = compileShader(g, GL_VERTEX_SHADER, vertex_source, "vertex") orelse return null;
    const fragment = compileShader(g, GL_FRAGMENT_SHADER, fragment_source, "fragment") orelse {
        g.delete_shader(vertex);
        return null;
    };
    const program = g.create_program();
    if (program == 0) {
        g.delete_shader(vertex);
        g.delete_shader(fragment);
        logOnce(LOG_FLIR_SHADER, "FLIR: glCreateProgram failed; FLIR shader pass disabled", .{});
        return null;
    }
    g.attach_shader(program, vertex);
    g.attach_shader(program, fragment);
    before_link(ctx, g, program);
    g.link_program(program);
    var status: i32 = 0;
    glq(@src());
    g.get_programiv(program, GL_LINK_STATUS, &status);
    g.delete_shader(vertex);
    g.delete_shader(fragment);
    if (status == 0) {
        var buf: [1024]u8 = undefined;
        logOnce(LOG_FLIR_SHADER, "FLIR: program link failed; FLIR shader pass disabled: {s}", .{programLog(g, program, &buf)});
        g.delete_program(program);
        return null;
    }
    return program;
}

/// Trim a GL info log to what one log line can hold.
fn infoLogText(buffer: []const u8) []const u8 {
    const end = std.mem.indexOfScalar(u8, buffer, 0) orelse buffer.len;
    return buffer[0..@min(end, 800)];
}

fn programLog(g: *const Gl, program: u32, buf: []u8) []const u8 {
    var length: i32 = 0;
    glq(@src());
    g.get_programiv(program, GL_INFO_LOG_LENGTH, &length);
    @memset(buf, 0);
    const n: i32 = @intCast(@min(@as(usize, @intCast(@max(length, 1))), buf.len));
    g.get_program_info_log(program, n, null, buf.ptr);
    return infoLogText(buf);
}

/// Same for a shader object: only the fetch differs (the log query is
/// object-kind specific), the trimming is shared with `programLog`.
fn shaderLog(g: *const Gl, shader: u32, buf: []u8) []const u8 {
    var length: i32 = 0;
    g.get_shaderiv(shader, GL_INFO_LOG_LENGTH, &length);
    @memset(buf, 0);
    const n: i32 = @intCast(@min(@as(usize, @intCast(@max(length, 1))), buf.len));
    g.get_shader_info_log(shader, n, null, buf.ptr);
    return infoLogText(buf);
}

fn compileShader(g: *const Gl, kind: u32, source: [:0]const u8, label: []const u8) ?u32 {
    const shader = g.create_shader(kind);
    if (shader == 0) {
        logOnce(LOG_FLIR_SHADER, "FLIR: glCreateShader({s}) failed; FLIR shader pass disabled", .{label});
        return null;
    }
    const pointers = [_][*]const u8{source.ptr};
    const lengths = [_]i32{@intCast(source.len)};
    g.shader_source(shader, 1, &pointers, &lengths);
    g.compile_shader(shader);
    var status: i32 = 0;
    g.get_shaderiv(shader, GL_COMPILE_STATUS, &status);
    if (status == 0) {
        var buf: [1024]u8 = undefined;
        logOnce(LOG_FLIR_SHADER, "FLIR: {s} shader compile failed; FLIR shader pass disabled: {s}", .{ label, shaderLog(g, shader, &buf) });
        g.delete_shader(shader);
        return null;
    }
    return shader;
}
