//! FLIR camera view (see `docs/FLIR_DESIGN.md`).
//!
//! The game's own "Infrared Mode" is only a tonemap grading (`color_mode = 1`
//! plus grain/vignette); no thermal pass exists behind it. This module adds one,
//! and only for views whose grading group is the IR signature (see
//! [`tonemap_params_ir`]) or, in a debug build, for the main view while F10 is
//! toggled on:
//!
//!   * `ir_tonemap_hook` (0x68b8c0) uploads the game's tonemap uniforms and
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
//!     hook then enables FLIR iff `state::flir_enabled_cfg(camera)` is Some.
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

use crate::fx::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows_sys::Win32::System::Memory::{
    VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS,
};

use crate::fault::{self, stage};
use crate::logging::{log_line, log_once};
use crate::memory::{
    bytes_match, image_backed, read_f64_at, read_u32_at, read_usize_at, readable,
    PTR_MIN,
};
use crate::offsets::*;
use crate::state::*;

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

macro_rules! gl_table {
    ($( $field:ident : $ty:ty ),* $(,)?) => {
        struct Gl {
            $( $field: $ty ),*
        }

        impl Gl {
            unsafe fn load() -> Option<Gl> {
                $(
                    let $field = {
                        let name = gl_entry_name(stringify!($field));
                        let raw = gl_symbol(&name);
                        if raw == 0 {
                            log_debug(|| {
                                concat!(
                                    "FLIR: GL entry point missing: ",
                                    stringify!($field),
                                    "; FLIR disabled"
                                )
                                .to_string()
                            });
                            return None;
                        }
                        std::mem::transmute::<usize, $ty>(raw)
                    };
                )*
                Some(Gl { $( $field ),* })
            }
        }
    };
}

gl_table! {
    create_shader: unsafe extern "system" fn(u32) -> u32,
    shader_source: unsafe extern "system" fn(u32, i32, *const *const i8, *const i32),
    compile_shader: unsafe extern "system" fn(u32),
    get_shaderiv: unsafe extern "system" fn(u32, u32, *mut i32),
    get_shader_info_log: unsafe extern "system" fn(u32, i32, *mut i32, *mut i8),
    create_program: unsafe extern "system" fn() -> u32,
    attach_shader: unsafe extern "system" fn(u32, u32),
    link_program: unsafe extern "system" fn(u32),
    get_programiv: unsafe extern "system" fn(u32, u32, *mut i32),
    get_program_info_log: unsafe extern "system" fn(u32, i32, *mut i32, *mut i8),
    delete_shader: unsafe extern "system" fn(u32),
    delete_program: unsafe extern "system" fn(u32),
    bind_attrib_location: unsafe extern "system" fn(u32, u32, *const i8),
    bind_frag_data_location: unsafe extern "system" fn(u32, u32, *const i8),
    get_uniform_location: unsafe extern "system" fn(u32, *const i8) -> i32,
    get_uniformfv: unsafe extern "system" fn(u32, i32, *mut f32),
    get_uniformiv: unsafe extern "system" fn(u32, i32, *mut i32),
    get_attached_shaders: unsafe extern "system" fn(u32, i32, *mut i32, *mut u32),
    get_shader_source: unsafe extern "system" fn(u32, i32, *mut i32, *mut i8),
    get_attrib_location: unsafe extern "system" fn(u32, *const i8) -> i32,
    use_program: unsafe extern "system" fn(u32),
    uniform1i: unsafe extern "system" fn(i32, i32),
    uniform1f: unsafe extern "system" fn(i32, f32),
    uniform2f: unsafe extern "system" fn(i32, f32, f32),
    uniform3f: unsafe extern "system" fn(i32, f32, f32, f32),
    uniform4fv: unsafe extern "system" fn(i32, i32, *const f32),
    uniform3i: unsafe extern "system" fn(i32, i32, i32, i32),
    uniform_matrix4fv: unsafe extern "system" fn(i32, i32, u8, *const f32),
    gen_textures: unsafe extern "system" fn(i32, *mut u32),
    delete_textures: unsafe extern "system" fn(i32, *const u32),
    bind_texture: unsafe extern "system" fn(u32, u32),
    active_texture: unsafe extern "system" fn(u32),
    tex_parameteri: unsafe extern "system" fn(u32, u32, i32),
    tex_image_3d: unsafe extern "system" fn(u32, i32, i32, i32, i32, i32, i32, u32, u32, *const c_void),
    tex_sub_image_3d: unsafe extern "system" fn(u32, i32, i32, i32, i32, i32, i32, i32, u32, u32, *const c_void),
    pixel_storei: unsafe extern "system" fn(u32, i32),
    bind_buffer: unsafe extern "system" fn(u32, u32),
    gen_vertex_arrays: unsafe extern "system" fn(i32, *mut u32),
    bind_vertex_array: unsafe extern "system" fn(u32),
    draw_arrays: unsafe extern "system" fn(u32, i32, i32),
    draw_elements: unsafe extern "system" fn(u32, i32, u32, *const c_void),
    get_integerv: unsafe extern "system" fn(u32, *mut i32),
    enable: unsafe extern "system" fn(u32),
    disable: unsafe extern "system" fn(u32),
    is_enabled: unsafe extern "system" fn(u32) -> u8,
    get_booleanv: unsafe extern "system" fn(u32, *mut u8),
    depth_mask: unsafe extern "system" fn(u8),
    color_mask: unsafe extern "system" fn(u8, u8, u8, u8),
    read_pixels: unsafe extern "system" fn(i32, i32, i32, i32, u32, u32, *mut c_void),
    bind_framebuffer: unsafe extern "system" fn(u32, u32),
    get_framebuffer_attachment_parameteriv: unsafe extern "system" fn(u32, u32, u32, *mut i32),
    get_tex_level_parameteriv: unsafe extern "system" fn(u32, i32, u32, *mut i32),
    get_vertex_attribiv: unsafe extern "system" fn(u32, u32, *mut i32),
    get_vertex_attrib_pointerv: unsafe extern "system" fn(u32, u32, *mut *const c_void),
    get_buffer_sub_data: unsafe extern "system" fn(u32, isize, isize, *mut c_void),
    get_buffer_parameteriv: unsafe extern "system" fn(u32, u32, *mut i32),
    gen_buffers: unsafe extern "system" fn(i32, *mut u32),
    buffer_data: unsafe extern "system" fn(u32, isize, *const c_void, u32),
    vertex_attrib_pointer: unsafe extern "system" fn(u32, i32, u32, u8, i32, *const c_void),
    enable_vertex_attrib_array: unsafe extern "system" fn(u32),
    tex_image_2d: unsafe extern "system" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void),
    // Compat-context attribute stacks: save/restore state without a query
    // (each glGet* forces a driver sync under Proton).
    push_attrib: unsafe extern "system" fn(u32),
    pop_attrib: unsafe extern "system" fn(),
    push_client_attrib: unsafe extern "system" fn(u32),
    pop_client_attrib: unsafe extern "system" fn(),
}

static GL: OnceLock<Option<Gl>> = OnceLock::new();

/// Driver queries issued by the plugin (each one can stall a threaded GL
/// driver); reported in the 5 s counters line.
static GL_QUERIES: AtomicU64 = AtomicU64::new(0);
/// Time spent inside the plugin's draw-thunk body (logging builds), microseconds.
#[cfg(feature = "logging")]
static HOOK_US: AtomicU64 = AtomicU64::new(0);
/// Part of HOOK_US spent in draws we substituted (vs. checking and skipping).
#[cfg(feature = "logging")]
static HOOK_SUB_US: AtomicU64 = AtomicU64::new(0);

/// Driver queries per source line since the last perf line (logging builds),
/// so a remaining sync can be traced to its call site.
#[cfg(feature = "logging")]
static GLQ_SITES: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());
#[cfg(feature = "logging")]
fn note_query_site(line: u32) {
    let mut sites = GLQ_SITES.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(e) = sites.iter_mut().find(|e| e.0 == line) {
        e.1 += 1;
    } else if sites.len() < 128 {
        sites.push((line, 1));
    }
}

/// Call a GL query entry point and count it.
macro_rules! glq {
    ($f:expr $(, $a:expr)* $(,)?) => {{
        GL_QUERIES.fetch_add(1, Ordering::Relaxed);
        #[cfg(feature = "logging")]
        note_query_site(line!());
        ($f)($($a),*)
    }};
}

// GL state tracked from the game's own calls through the GLEW slots, so the
// per-draw path never has to ask the driver. -1 = unknown (query once, then
// tracked). Only the render thread issues GL.
static TRACK_PROGRAM: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);
static TRACK_IBO: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);
static TRACK_ACTIVE_TEX: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);
static ORIG_GL_USE_PROGRAM: AtomicUsize = AtomicUsize::new(0);
static ORIG_GL_BIND_BUFFER: AtomicUsize = AtomicUsize::new(0);
static ORIG_GL_ACTIVE_TEXTURE: AtomicUsize = AtomicUsize::new(0);
static ORIG_GL_BIND_VERTEX_ARRAY: AtomicUsize = AtomicUsize::new(0);
static ORIG_GL_VIEWPORT: AtomicUsize = AtomicUsize::new(0);
/// Game's GL_PIXEL_UNPACK_BUFFER binding (-1 = unknown) and viewport.
static TRACK_UNPACK_PBO: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);
static TRACK_VIEWPORT: Mutex<Option<[i32; 4]>> = Mutex::new(None);

/// Tracked value, or one counted query (then tracked) while unknown.
unsafe fn tracked(gl: &'static Gl, slot: &std::sync::atomic::AtomicI64, pname: u32) -> i32 {
    let v = slot.load(Ordering::Relaxed);
    if v >= 0 {
        return v as i32;
    }
    let mut out = 0i32;
    glq!(gl.get_integerv, pname, &mut out);
    // Only cache once the game's own calls are being tracked (slot hooked).
    let hooked = match pname {
        GL_CURRENT_PROGRAM => ORIG_GL_USE_PROGRAM.load(Ordering::Relaxed) != 0,
        GL_ELEMENT_ARRAY_BUFFER_BINDING => ORIG_GL_BIND_BUFFER.load(Ordering::Relaxed) != 0,
        GL_PIXEL_UNPACK_BUFFER_BINDING => ORIG_GL_BIND_BUFFER.load(Ordering::Relaxed) != 0,
        GL_ACTIVE_TEXTURE => ORIG_GL_ACTIVE_TEXTURE.load(Ordering::Relaxed) != 0,
        _ => false,
    };
    if hooked {
        slot.store(out as i64, Ordering::Relaxed);
    }
    out
}

pub(crate) extern "C" fn gl_use_program_hook(program: u32) {
    let original = ORIG_GL_USE_PROGRAM.load(Ordering::Relaxed);
    if original != 0 {
        let f: unsafe extern "system" fn(u32) = unsafe { std::mem::transmute(original) };
        unsafe { f(program) };
    }
    TRACK_PROGRAM.store(program as i64, Ordering::Relaxed);
}

pub(crate) extern "C" fn gl_bind_buffer_hook(target: u32, buffer: u32) {
    let original = ORIG_GL_BIND_BUFFER.load(Ordering::Relaxed);
    if original != 0 {
        let f: unsafe extern "system" fn(u32, u32) = unsafe { std::mem::transmute(original) };
        unsafe { f(target, buffer) };
    }
    if target == 0x8893 {
        TRACK_IBO.store(buffer as i64, Ordering::Relaxed);
    } else if target == GL_PIXEL_UNPACK_BUFFER {
        TRACK_UNPACK_PBO.store(buffer as i64, Ordering::Relaxed);
    }
}

pub(crate) extern "C" fn gl_viewport_hook(x: i32, y: i32, w: i32, h: i32) {
    let original = ORIG_GL_VIEWPORT.load(Ordering::Relaxed);
    if original != 0 {
        let f: unsafe extern "system" fn(i32, i32, i32, i32) = unsafe { std::mem::transmute(original) };
        unsafe { f(x, y, w, h) };
    }
    if let Ok(mut v) = TRACK_VIEWPORT.lock() {
        *v = Some([x, y, w, h]);
    }
}

/// Current viewport: tracked from the game's `glViewport` (IAT), else queried.
unsafe fn current_viewport(gl: &'static Gl) -> [i32; 4] {
    if let Some(v) = *TRACK_VIEWPORT.lock().unwrap_or_else(|p| p.into_inner()) {
        return v;
    }
    let mut v = [0i32; 4];
    glq!(gl.get_integerv, GL_VIEWPORT, v.as_mut_ptr());
    v
}

static ORIG_GL_BIND_TEXTURE: AtomicUsize = AtomicUsize::new(0);
/// Last GL_TEXTURE_2D bound per texture unit (game's glBindTexture, IAT).
static TEX_UNITS: [AtomicU32; 16] = [const { AtomicU32::new(0) }; 16];

pub(crate) extern "C" fn gl_bind_texture_hook(target: u32, texture: u32) {
    let original = ORIG_GL_BIND_TEXTURE.load(Ordering::Relaxed);
    if original != 0 {
        let f: unsafe extern "system" fn(u32, u32) = unsafe { std::mem::transmute(original) };
        unsafe { f(target, texture) };
    }
    if target == GL_TEXTURE_2D {
        let unit = TRACK_ACTIVE_TEX.load(Ordering::Relaxed) - GL_TEXTURE0 as i64;
        if (0..16).contains(&unit) {
            TEX_UNITS[unit as usize].store(texture, Ordering::Relaxed);
        }
    }
}

pub(crate) extern "C" fn gl_active_texture_hook(texture: u32) {
    let original = ORIG_GL_ACTIVE_TEXTURE.load(Ordering::Relaxed);
    if original != 0 {
        let f: unsafe extern "system" fn(u32) = unsafe { std::mem::transmute(original) };
        unsafe { f(texture) };
    }
    TRACK_ACTIVE_TEX.store(texture as i64, Ordering::Relaxed);
}

pub(crate) extern "C" fn gl_bind_vertex_array_hook(vao: u32) {
    let original = ORIG_GL_BIND_VERTEX_ARRAY.load(Ordering::Relaxed);
    if original != 0 {
        let f: unsafe extern "system" fn(u32) = unsafe { std::mem::transmute(original) };
        unsafe { f(vao) };
    }
    // The element binding is VAO state: unknown until the next bind/query.
    TRACK_IBO.store(-1, Ordering::Relaxed);
}
static GL_BASE: AtomicUsize = AtomicUsize::new(0);
/// Per-hook switches (`infrared_disable_flir_draw` / `_matrix` / `_fbo` next to
/// the DLL): each GL dispatch slot can be left untouched on its own, so a
/// rendering problem can be bisected down to a single thunk without a rebuild.
/// Read at install time; `ensure_gl_slots` runs again from the render thread, so
/// the decision has to live in statics.
static DRAW_THUNK_DISABLED: AtomicBool = AtomicBool::new(false);
static MATRIX_THUNK_DISABLED: AtomicBool = AtomicBool::new(false);
static FBO_THUNK_DISABLED: AtomicBool = AtomicBool::new(false);

/// GL table, or `None` when a required entry point cannot be resolved. The
/// first call also retries the GLEW dispatch-slot patches: if the plugin was
/// injected before GLEW initialised, the slots were still null at install
/// time and are only usable from the render thread anyway.
fn gl() -> Option<&'static Gl> {
    GL.get_or_init(|| {
        unsafe { ensure_gl_slots() };
        unsafe { Gl::load() }
    })
    .as_ref()
}

fn gl_entry_name(field: &str) -> Vec<u8> {
    // `gen_vertex_arrays` -> `glGenVertexArrays`: the GL entry points are
    // exported (or returned by wglGetProcAddress) under exactly those names.
    let mut name = String::with_capacity(field.len() + 2);
    name.push_str("gl");
    for part in field.split('_') {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            name.extend(first.to_uppercase());
            if first.is_ascii_digit() {
                // `2d` / `3d` are spelled `2D` / `3D` in the GL names.
                name.extend(chars.flat_map(char::to_uppercase));
            } else {
                name.extend(chars);
            }
        }
    }
    name.push('\0');
    name.into_bytes()
}

fn gl_symbol(name: &[u8]) -> usize {
    static OGL: OnceLock<usize> = OnceLock::new();
    static WGL_GET_PROC: OnceLock<usize> = OnceLock::new();
    unsafe {
        let module = *OGL.get_or_init(|| LoadLibraryA(b"opengl32.dll\0".as_ptr()) as usize);
        if module == 0 {
            return 0;
        }
        if let Some(direct) = GetProcAddress(module as _, name.as_ptr()) {
            return direct as usize;
        }
        let get_proc = *WGL_GET_PROC.get_or_init(|| {
            GetProcAddress(module as _, b"wglGetProcAddress\0".as_ptr()).map_or(0, |f| f as usize)
        });
        if get_proc == 0 {
            return 0;
        }
        let f: unsafe extern "system" fn(*const i8) -> *const c_void = std::mem::transmute(get_proc);
        // wglGetProcAddress returns small sentinel values (1, 2, 3, -1) for
        // functions it does not know; never accept those as addresses.
        let p = f(name.as_ptr() as *const i8) as usize;
        if p > 0x10000 {
            p
        } else {
            0
        }
    }
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
const BLOCK_VS: &str = r#"#version 330 core
in vec3 vertex_position_in;
in vec3 vertex_normal_in;
in vec3 vertex_color_in;
uniform int has_color;

uniform mat4 mat_view_proj;
uniform mat4 mat_world;
// Chunk draws: identity. Component draws: inverse(body mat) * component mat, so
// flir_pos/flir_lnormal are body-local for the per-fragment voxel lookup.
uniform mat4 comp_to_body;

out vec3 flir_normal;
out vec3 flir_pos;
out vec3 flir_lnormal;
out float log_z;
out float flir_paint;

void main()
{
    // Paint brightness (vertex colour, display space) for the skin sun gain.
    flir_paint = has_color == 1 ? dot(clamp(vertex_color_in, 0.0, 1.0), vec3(0.2126, 0.7152, 0.0722)) : 0.5;
    // The game's own opaque VS writes the WORLD normal into the G-buffer
    // (`(mat_world * vec4(vertex_normal_in, 0.0)).xyz`), so the substitute has
    // to write the same space: attachment 0 is a world-space normal.
    flir_normal = (mat_world * vec4(vertex_normal_in, 0.0)).xyz;
    flir_pos = (comp_to_body * vec4(vertex_position_in, 1.0)).xyz;
    flir_lnormal = mat3(comp_to_body) * vertex_normal_in;
    gl_Position = mat_view_proj * (mat_world * vec4(vertex_position_in, 1.0));
    // depth_utils.glslh (USE_LOG2 + DEPTH_LOG_IN_FRAGMENT): the game writes log
    // depth, so our substitute draw must write the identical value or the
    // depth buffer the later deferred passes read would be corrupted.
    log_z = gl_Position.w * 0.01 + 1.0;
}
"#;

const BLOCK_FS: &str = r#"#version 330 core
in vec3 flir_normal;
in vec3 flir_pos;
in vec3 flir_lnormal;
in float log_z;
in float flir_paint;

uniform ivec3 grid_min;
uniform ivec3 grid_dims;
uniform float enc_min;
uniform float enc_max;
uniform sampler3D tex_temps;
uniform int normal_encoded;
// Component draws: the component's own origin voxel (body-local); chunk draws
// set has_comp_voxel = 0.
uniform ivec3 comp_voxel;
uniform int has_comp_voxel;

out vec4 flir_out0;
out vec4 flir_out1;
out vec4 flir_out2;

// Temperature of a body-local voxel, EMPTY when empty/outside the grid. Empty
// voxels are NaN by contract (see BodyGrid), and 0.0 is an unwritten texel:
// neither may read as a cold vehicle (no marker -> composite shows ambient).
const float EMPTY = -1000.0;
float cell_temp(ivec3 voxel)
{
    ivec3 local = voxel - grid_min;
    if (any(lessThan(local, ivec3(0))) || any(greaterThanEqual(local, grid_dims)))
        return EMPTY;
    float t = texelFetch(tex_temps, local, 0).r;
    return (isnan(t) || t <= -998.0 || t == 0.0) ? EMPTY : t;
}

void main()
{
    // FC = log2(far * C + 1), far 20100, C 0.01
    gl_FragDepth = log2(max(log_z, 1e-6)) / 7.658227526203386;

    // Voxel centres sit at voxel*0.25 body-local and faces at +-0.125, so a
    // surface point moved 0.0625 m back along its normal is inside its voxel.
    // Per fragment (not per vertex) so the paint is square 0.25 m cells.
    ivec3 voxel = ivec3(floor((flir_pos - normalize(flir_lnormal) * 0.0625) / 0.25 + 0.5));
    float t = cell_temp(voxel);
    // Geometry outside its part's listed voxels (a microcontroller's footprint
    // beyond its one definition voxel, mesh overhang): borrow the average of
    // the occupied cells around it, then the component's own origin cell.
    if (t == EMPTY)
    {
        float sum = 0.0;
        float n = 0.0;
        for (int dz = -1; dz <= 1; dz++)
        for (int dy = -1; dy <= 1; dy++)
        for (int dx = -1; dx <= 1; dx++)
        {
            float u = cell_temp(voxel + ivec3(dx, dy, dz));
            if (u != EMPTY) { sum += u; n += 1.0; }
        }
        if (n > 0.0)
            t = sum / n;
        else if (has_comp_voxel == 1)
            t = cell_temp(comp_voxel);
    }
    bool known = t != EMPTY;
    float enc = known ? clamp((t - enc_min) / max(enc_max - enc_min, 1.0), 0.0, 1.0) : 0.0;

    // G-buffer location 0 = gnormal_light_factor_out: the game writes the full
    // WORLD-space normal in xyz and the light factor in .w
    // (`vec4(normalize(vertex_normal_out), light_factor)` in opaque.glslf), so we
    // write the identical packing. The vehicle marker is a light factor of
    // exactly -1.0: no game shader writes a negative light factor, and light.glslf
    // only thresholds .w (< 0.25 -> 0), so it cannot leak into the game's passes.
    vec3 n = normalize(flir_normal);
    if (normal_encoded == 1)
    {
        // Normalized-attachment fallback (the game uses an RGBA16F normal
        // attachment): only xy can be packed, so the marker moves to gcolor alpha
        // (0.5 is exact in RGBA8; the game writes 1.0).
        flir_out0 = vec4(n.xy * 0.5 + 0.5, 1.0, 1.0);
        flir_out1 = vec4(enc, flir_paint, 0.0, known ? 0.5 : 1.0);
    }
    else
    {
        flir_out0 = vec4(n, known ? -1.0 : 1.0);
        flir_out1 = vec4(enc, flir_paint, 0.0, 1.0);
    }
    // G-buffer location 2 = gvelocity_out: a substitute draw has no motion
    // history of its own, and a stale value would make temporal AA reproject
    // the composite with some other pass's velocity (ghosting around vehicles).
    flir_out2 = vec4(0.0);
}
"#;

// The game's own tonemap quad geometry (tonemap.glslv): the texcoords the game
// feeds define the exact G-buffer region this view shows.
const COMPOSITE_VS: &str = r#"#version 330 core
in vec3 vertex_position_in;
in vec2 vertex_coord0_in;
out vec2 uv;
uniform mat4 mat_view_proj;
void main()
{
    uv = vertex_coord0_in;
    gl_Position = mat_view_proj * vec4(vertex_position_in, 1.0);
}
"#;

const COMPOSITE_FS: &str = r#"#version 330 core
in vec2 uv;
out vec4 color_out;

uniform sampler2D tex_normal;
uniform sampler2D tex_color;
uniform sampler2D tex_depth;
uniform int has_depth;
uniform int normal_encoded;

uniform vec3 sun_dir;
uniform float sun_valid;
uniform float day_pct;
uniform float clear_sky;
// Deferred light pass state of this view: inverse view-projection (render
// space) and the sea plane's render-space y (world y 0 - graphics_offset.y).
uniform mat4 mat_inv;
uniform int has_inv;
uniform float water_y;
uniform float ambient_c;      // sea-level air: terrain, water, people
uniform float vehicle_air_c;  // air at the vehicles/camera
uniform sampler2D tex_water;  // game's ocean-surface log depth (depth_ocean)
uniform int has_water;
uniform vec4 plume_a[8];      // jet plume start (render space), excess C
uniform vec4 plume_b[8];      // unit flow direction, nozzle radius
uniform int plume_n;
const float DETAIL = 0.12;   // detail overlay strength in display units
uniform float sun_soak_c;
uniform vec3 tint;
uniform int black_hot;
uniform float tmin;
uniform float tmax;
uniform float enc_min;
uniform float enc_max;
uniform float sky_c;
uniform float sky_depth_m;

// depth_utils.glslh: log depth, near 0.025, far 20100, C 0.01
float decode_linear_depth(float d)
{
    const float FC = 7.658227526203386; // log2(20100.0 * 0.01 + 1.0)
    return (pow(2.0, d * FC) - 1.0) * 100.0;
}

// Attachment 0 is gnormal_light_factor: a full WORLD-space normal in xyz and the
// light factor in .w (`vec4(normalize(vertex_normal_out), light_factor)` in
// opaque.glslf; the game's light pass reads normalize(gnormal.xyz) and gnormal.a).
// z is a real normal component - reconstructing it from xy would lose the sign
// and give every downward-facing surface the wrong sun soak.
vec3 gbuffer_normal(vec4 raw)
{
    if (normal_encoded == 0)
    {
        float len2 = dot(raw.xyz, raw.xyz);
        // NaN-safe: an unwritten normal falls back to up instead of garbage.
        return len2 > 1e-8 ? raw.xyz * inversesqrt(len2) : vec3(0.0, 1.0, 0.0);
    }
    // Normalized-attachment fallback: packed xy, z reconstructed (positive).
    vec2 nxy = raw.xy * 2.0 - 1.0;
    float len2 = dot(nxy, nxy);
    if (!(len2 <= 1.0))
    {
        return vec3(0.0, 1.0, 0.0);
    }
    return vec3(nxy, sqrt(1.0 - len2));
}

const float TWO_PI = 6.283185307;

// FlightGear Advanced Weather diurnal surface cycle (Nasal/local_weather/
// local_weather.nas): f = 0.5 * (1 - cos(day * 2pi - lag)), lag in radians =
// thermal inertia (soil 0.4, water 0.9, vegetation/rock 0).
float diurnal(float lag)
{
    return 0.5 * (1.0 - cos(day_pct * TWO_PI - lag));
}

// Terrain/props surface temperature from the G-buffer albedo (linear gcolor):
// - material class from colour (green = vegetation, otherwise soil/rock),
//   with FlightGear's delta-T cycles (veg -5..+5, soil -5..+5 lagged,
//   rock -7..+7) scaled by the clear-sky factor;
// - absorbed sun = (1 - albedo) * incidence: FlightGear's IR mixes in the
//   inverted brightness for the same reason. Vegetation transpires, so it
//   keeps only a third of the solar gain (cool leaves over warm trunks/soil).
// Bright sand and dark rock converge at midday through the shared ambient and
// soak terms, which is the thermal washout of hot deserts.
float terrain_temp(vec3 albedo_lin, float incidence)
{
    vec3 c = pow(max(albedo_lin, vec3(0.0)), vec3(1.0 / 2.2));
    float lum = dot(c, vec3(0.2126, 0.7152, 0.0722));
    float green = c.g - max(c.r, c.b);
    float veg = smoothstep(0.02, 0.10, green);
    float rock = (1.0 - veg) * (1.0 - smoothstep(0.08, 0.25, max(c.r, max(c.g, c.b)) - min(c.r, min(c.g, c.b))));
    float d_veg = -5.0 + 10.0 * diurnal(0.0);
    float d_soil = -5.0 + 10.0 * diurnal(0.4);
    float d_rock = -7.0 + 14.0 * diurnal(0.0);
    float delta = mix(mix(d_soil, d_rock, rock), d_veg, veg) * clear_sky;
    float soak = (1.0 - lum) * incidence * sun_soak_c * clear_sky * mix(1.0, 0.35, veg);
    return ambient_c + delta + soak;
}

// The block pass marks a known vehicle voxel with a light factor of exactly -1.0
// (no game shader writes a negative light factor); a normalized attachment cannot
// store that, so there the marker is gcolor alpha 0.5 (the game writes 1.0).
bool is_vehicle(vec4 normal, vec4 colour)
{
    return normal_encoded == 1 ? abs(colour.a - 0.5) < 0.02 : normal.w < -0.5;
}

void main()
{
    // G-buffer 0 = gnormal_light_factor, 1 = gcolor (verified: glDrawBuffers is
    // the fixed COLOR_ATTACHMENT0..4 list and opaque declares the outputs in
    // that order).
    // The target may be smaller than the G-buffer (camera feed 128x128 vs
    // 1152x640): sample at the game's own quad texcoords, as its tonemap does.
    vec2 guv = uv;
    vec4 normal = texture(tex_normal, guv);
    vec4 colour = texture(tex_color, guv);

    vec3 n = gbuffer_normal(normal);
    float incidence = max(0.0, dot(n, normalize(sun_dir))) * sun_valid;
    float dist = has_depth == 1 ? decode_linear_depth(texture(tex_depth, guv).r) : -1.0;
    bool sky = has_depth == 1 && dist > sky_depth_m;
    bool marked_any = is_vehicle(normal, colour) || (normal_encoded == 0 && normal.w < -0.5);

    // View ray (render space) for the sea plane and the sun disc.
    vec3 eye = vec3(0.0);
    vec3 ray = vec3(0.0, 0.0, -1.0);
    vec3 fwd = ray;
    if (has_inv == 1)
    {
        vec4 a = mat_inv * vec4(guv * 2.0 - 1.0, -1.0, 1.0);
        vec4 b = mat_inv * vec4(guv * 2.0 - 1.0, 1.0, 1.0);
        vec4 c = mat_inv * vec4(0.0, 0.0, -1.0, 1.0);
        vec4 e = mat_inv * vec4(0.0, 0.0, 1.0, 1.0);
        eye = a.xyz / a.w;
        ray = normalize(b.xyz / b.w - eye);
        fwd = normalize(e.xyz / e.w - c.xyz / c.w);
    }
    // The game's moon/sun sprites sit ~18 km out: unmarked geometry that high
    // above the sea is sky, not terrain.
    if (has_inv == 1 && has_depth == 1 && !sky && !marked_any
        && eye.y + ray.y * dist / max(dot(ray, fwd), 1e-3) > water_y + 3000.0)
        sky = true;
    // Water: the ocean is forward-rendered after the G-buffer, so the
    // G-buffer holds the sea floor. IR does not penetrate water: anything
    // past the sea plane (or everything, from below it) is the surface.
    float water = 0.0;
    if (has_inv == 1)
    {
        if (eye.y < water_y)
            water = 1.0;
        else if (ray.y < -1e-4)
        {
            float t = (water_y - eye.y) / ray.y;
            float plane_depth = t * dot(ray, fwd);
            if (sky || has_depth == 0 || dist > plane_depth * 1.006 + 0.2)
                water = 1.0;
        }
        // The game's own test (light.glslf fog): the rendered ocean surface,
        // waves included, in front of the scene. Same encoding as gdepth.
        if (has_water == 1 && eye.y >= water_y && textureSize(tex_water, 0).x > 32)
        {
            // Where no ocean was drawn the texture holds its clear value
            // (0 = "depth 0", which would read as water in front of
            // everything, sky included): only trust drawn texels, and leave
            // sky pixels to the plane test.
            float raw = texture(tex_water, guv).r;
            if (!sky && raw > 1e-6 && raw < 0.999)
                water = decode_linear_depth(raw) < dist ? 1.0 : 0.0;
        }
        // Unmarked geometry at the waterline (surf foam/bubble particles,
        // wet sand) flickers against the plane test: it reads as water.
        if (water < 0.5 && !marked_any && !sky && has_depth == 1 && dist < 300.0)
        {
            float py = eye.y + ray.y * dist / max(dot(ray, fwd), 1e-3);
            if (py < water_y + 0.4)
                water = 1.0;
        }
    }

    float temp;
    float sun_glow = 0.0; // display-space halo (camera bloom), 0..1
    if (water > 0.5)
    {
        // FlightGear water cycle (-1..+1, lag 0.9) a little under the air,
        // reflecting the cold sky at grazing angles (Fresnel).
        float t_water = ambient_c - 2.0 + (-1.0 + 2.0 * diurnal(0.9)) * clear_sky;
        float fres = eye.y < water_y ? 0.0 : 0.02 + 0.98 * pow(1.0 - abs(ray.y), 5.0);
        temp = mix(t_water, sky_c, fres * 0.35);
    }
    else if (normal_encoded == 0 && normal.w < -4.5 && normal.w > -5.5)
    {
        // Gas particle (exhaust/smoke/steam) from the gas pass.
        temp = vehicle_air_c + colour.r * 1000.0;
    }
    else if (normal_encoded == 0 && normal.w < -2.5 && normal.w > -3.5)
    {
        // Skinned draw (person/animal), marked via its light_factor: clothed
        // skin sits a little above the air, never below ~25 C.
        temp = max(mix(ambient_c, 33.0, 0.7), 25.0);
    }
    else if (is_vehicle(normal, colour))
    {
        // Simulated block temperature plus the painted skin's own sun gain
        // (thin paint/skin heats in minutes, the block mass in hours): dark
        // paint on the sunlit side of a hull reads warmer. colour.g carries
        // the block's paint brightness (block pass).
        // The surface skin (paint over thin plate) settles at the same sun soak
        // as terrain within minutes; the block's bulk temperature (mass-
        // weighted, slow) only shows once it is hotter than that skin.
        float absorb = 1.0 - 0.8 * clamp(colour.g, 0.0, 1.0);
        float bulk = enc_min + colour.r * (enc_max - enc_min);
        temp = max(bulk, vehicle_air_c + absorb * incidence * clear_sky * sun_soak_c);
    }
    else
    {
        if (sky)
        {
            temp = sky_c;
            if (has_inv == 1 && sun_valid > 0.5)
            {
                // Sun along the light pass's own direction: ~2.5 deg saturated
                // core (the game's bright core) plus a sensor-bloom halo that
                // falls off over ~8 deg.
                // Drawn sprite sits below the light direction (sun.rs
                // SUN_SPRITE_DROP, measured in game).
                vec3 disc = normalize(normalize(sun_dir) - vec3(0.0, 0.07, 0.0));
                float ang = acos(clamp(dot(ray, disc), -1.0, 1.0));
                float core = 1.0 - smoothstep(0.036, 0.048, ang);
                temp = mix(temp, 1500.0, core * clear_sky);
                sun_glow = (0.35 * exp(-max(ang - 0.04, 0.0) / 0.035)
                    + 0.10 * exp(-max(ang - 0.04, 0.0) / 0.14)) * clear_sky;
            }
        }
        else
        {
            temp = terrain_temp(colour.rgb, incidence);
        }
    }

    // Jet exhaust: hot gas cones along each nozzle's flow axis, in front of
    // the scene, cooling and widening downstream.
    if (has_inv == 1)
    {
        float scene = (has_depth == 1 && !sky) ? dist : 1e9;
        for (int i = 0; i < plume_n; ++i)
        {
            vec3 p0 = plume_a[i].xyz;
            vec3 d = plume_b[i].xyz;
            float excess = plume_a[i].w;
            float r0 = plume_b[i].w;
            float len = 3.0 + max(excess - 100.0, 0.0) / 100.0;
            vec3 w = eye - p0;
            float b = dot(ray, d);
            float den = max(1.0 - b * b, 1e-4);
            float ta = clamp((dot(d, w) - b * dot(ray, w)) / den, 0.0, len);
            float s = dot(p0 + d * ta - eye, ray);
            if (s <= 0.0 || s * dot(ray, fwd) > scene)
                continue;
            float r = r0 + 0.08 * ta;
            float q = length(eye + ray * s - (p0 + d * ta)) / r;
            if (q < 1.0)
                temp = max(temp, vehicle_air_c + excess * exp(-3.0 * ta / len) * (1.0 - q * q));
        }
    }

    float v = clamp((temp - tmin) / max(tmax - tmin, 0.001), 0.0, 1.0);
    // Keep some surface detail under the temperature paint (sensor contrast
    // from emissivity and shape): paint/albedo brightness and a soft
    // view-facing shade. People and blocks of one temperature stay readable.
    if (water < 0.5 && !sky && !(normal_encoded == 0 && normal.w < -4.5 && normal.w > -5.5))
    {
        float lum;
        if (is_vehicle(normal, colour))
            lum = clamp(colour.g, 0.0, 1.0);
        else
            lum = dot(pow(clamp(colour.rgb, 0.0, 1.0), vec3(1.0 / 2.2)), vec3(0.299, 0.587, 0.114));
        float shade = has_inv == 1 ? max(dot(n, -ray), 0.0) : 0.5 + 0.5 * n.y;
        // Scaled by the signal: below the window stays black, so albedo
        // (painted/lit-looking surfaces) cannot show through on its own.
        v = clamp(v + DETAIL * ((lum - 0.5) * 0.6 + (shade - 0.6)) * smoothstep(0.0, 0.25, v), 0.0, 1.0);
    }
    v = max(v, sun_glow);
    // Black hot = colour inversion of the tinted white-hot picture (like an
    // image editor's invert), not a flip of the brightness inside the tint.
    vec3 rgb = v * tint;
    color_out = vec4(black_hot == 1 ? vec3(1.0) - rgb : rgb, 1.0);
}
"#;

// ---------------------------------------------------------------------------
// Runtime state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct BlockProgram {
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
}

/// The composite program and its uniform locations. It draws into whichever
/// framebuffer the game has bound at the tonemap quad (the tonemap output RT),
/// so there is no render target of our own.
#[derive(Clone, Copy)]
struct Composite {
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
}

/// The opaque G-buffer target: attachment 0 is `gnormal_light_factor`, 1 is
/// `gcolor` (the fixed `glDrawBuffers` list and the opaque shader's output order
/// are verified in RE_NOTES §1.3.4/§1.2.2). `normal_encoded` records whether
/// attachment 0 is a normalized format: the packed normal.xy is then decoded
/// with `*2-1`, and the vehicle marker has to live in `gcolor` alpha instead of
/// the light-factor channel (a normalized attachment cannot hold -1).
#[derive(Clone, Copy, Default)]
struct Gbuffer {
    fbo: u32,
    tex_normal: u32,
    tex_color: u32,
    depth: u32,
    /// Texel size of the colour-0 attachment: diagnostics only. The composite
    /// samples the G-buffer with normalised UVs, so it does not require the
    /// tonemap viewport to have the same size, and the view a G-buffer belongs to
    /// is decided by the pairing learned at that view's tonemap quad (`Targets`),
    /// never by a size comparison.
    size: [i32; 2],
    normal_encoded: bool,
}

struct SavedState {
    program: i32,
    active_texture: i32,
}

/// Our 3D texture currently bound on FLIR_TEXTURE_UNIT (0: unknown/none). No
/// game shader samples a 3D texture, so it stays bound between draws instead
/// of a bind + unbind per substituted draw.
static LAST_3D: AtomicU32 = AtomicU32::new(0);

/// Uniform values last uploaded to each block program. Uniforms persist per
/// program and only our substitute uses these programs, so unchanged values
/// (same view, same body for consecutive chunks) skip the GL call.
#[derive(Clone, Copy, PartialEq)]
struct BlockUni {
    vp: ([f32; 16], u8),
    world: ([f32; 16], u8),
    comp: [f32; 16],
    min: [i32; 3],
    dims: [u32; 3],
    normal_encoded: bool,
    comp_voxel: [i32; 3],
    is_comp: bool,
}
static BLOCK_UNI: Mutex<Vec<(u32, BlockUni)>> = Mutex::new(Vec::new());

/// Set while we re-issue the draw with our own program, so nothing (including a
/// nested GL hook) can substitute into our own draw.
static IN_SUBSTITUTE: AtomicBool = AtomicBool::new(false);
/// A panic caught in the block pass disables it for the session: the GL state of
/// a half-finished substitute draw cannot be trusted.
static BLOCK_FAILED: AtomicBool = AtomicBool::new(false);

/// Keyed by the game's opaque-program id: attribute locations are per program,
/// so a hit needs no GL queries. A failed build is cached as `None` so a broken
/// shader is not rebuilt (and its info log not rewritten) on every draw.
static BLOCK_PROGRAMS: Mutex<Vec<(u32, Option<BlockProgram>)>> = Mutex::new(Vec::new());
/// Programs seen drawing a mapped vehicle chunk mesh in the G-buffer pass. The
/// opaque variants share the attribute/uniform layout, so a program that has
/// `vertex_position_in`, `vertex_normal_in` and `mat_world` qualifies.
/// program -> has the opaque layout. Negatives are cached too: an uncached
/// rejection cost three driver queries on every draw of that program.
static OPAQUE_PROGRAMS: LazyLock<Mutex<HashMap<u32, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::default()));
static COMPOSITE: Mutex<Option<Composite>> = Mutex::new(None);
/// Keyed by the chunk mesh's GL index buffer id (`mesh+0x64`). Built on the game
/// thread; the render thread only looks up the published map.
static MESH_BODY: LazyLock<Mutex<HashMap<u32, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::default()));
/// Lock-free mirror of "the published mesh->body map is empty", so the per-draw
/// gate can skip the whole block pass without taking the map lock or issuing a
/// single GL query (an empty map means no draw can ever be substituted).
static MESH_BODY_EMPTY: AtomicBool = AtomicBool::new(true);

/// One R32F volume per body. `frame` is the submit counter of the last upload,
/// which throttles uploads to one per body per `FLIR_UPLOAD_INTERVAL_FRAMES`.
/// `min`/`dims` describe the texture that is actually resident.
#[derive(Clone, Copy)]
struct GridTexture {
    texture: u32,
    version: u64,
    min: [i32; 3],
    dims: [u32; 3],
    frame: u64,
}

static GRID_TEXTURES: LazyLock<Mutex<HashMap<usize, GridTexture>>> =
    LazyLock::new(|| Mutex::new(HashMap::default()));
/// Staging copy used for the volume upload. The render thread is its only user,
/// so holding it across the `glTexImage3D` cannot block the sim thread, and the
/// simulated grid itself is never cloned.
static UPLOAD_BUF: Mutex<Vec<f32>> = Mutex::new(Vec::new());

/// Last `mat_view_proj` / `mat_world` uploaded to each program (raw floats and
/// transpose flag), plus the program's two uniform locations (looked up once).
/// A uniform is program state, so this is exactly what a draw with that program
/// uses - without a per-draw `glGetUniformfv`. One lock per matrix upload: the
/// hook runs for every upload in the frame while FLIR is on.
#[derive(Clone, Copy, Default)]
struct ProgMats {
    locs: (i32, i32),
    /// mat_view_proj_inverse location of the deferred light program (else -1).
    light_inv: i32,
    vp: Option<([f32; 16], u8)>,
    world: Option<([f32; 16], u8)>,
}
/// GL object names are small sequential integers: no need for SipHash.
#[derive(Default)]
struct IdHasher(u64);
impl std::hash::Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 << 8 | b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_u32(&mut self, v: u32) {
        self.0 = (v as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}
type IdMap<V> = std::collections::HashMap<u32, V, std::hash::BuildHasherDefault<IdHasher>>;
static PROG_MATS: LazyLock<Mutex<IdMap<ProgMats>>> = LazyLock::new(|| Mutex::new(IdMap::default()));
/// Raw 16 floats of the last `mat_view_proj` / `mat_world` upload, replayed with
/// the same transpose flag the game used (identical transform).
static VIEW_PROJ: Mutex<[f32; 16]> = Mutex::new([0.0; 16]);
static VIEW_PROJ_TRANSPOSE: AtomicU32 = AtomicU32::new(0);
static VIEW_PROJ_VALID: AtomicBool = AtomicBool::new(false);
static WORLD_MATRIX: Mutex<[f32; 16]> = Mutex::new([0.0; 16]);
static WORLD_TRANSPOSE: AtomicU32 = AtomicU32::new(0);
static WORLD_VALID: AtomicBool = AtomicBool::new(false);

/// G-buffers by framebuffer id: attachments and formats, probed once when the
/// target is first bound (`capture_gbuffer`); the draw thunk only ever reads
/// this map, it never queries the framebuffer it is bound to.
static GBUFFERS: LazyLock<Mutex<HashMap<u32, Gbuffer>>> =
    LazyLock::new(|| Mutex::new(HashMap::default()));
/// Framebuffers already probed and found NOT to be a G-buffer (shadow maps,
/// tonemap targets, ...), so a pass that binds one does not re-query its
/// attachments on every draw.
static NOT_GBUFFERS: LazyLock<Mutex<std::collections::HashSet<u32>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));
/// Every bloom/shadow/feed target is probed once; at 32 a busy frame cycled the
/// list and re-probed (4+ driver queries) on every bind.
const NOT_GBUFFER_LIMIT: usize = 1024;
/// A composite that cannot be built (or whose target cannot take it) disables
/// the FLIR view: the game's own image is then kept (fail closed).
static COMPOSITE_FAILED: AtomicBool = AtomicBool::new(false);

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
#[derive(Clone, Copy)]
struct TargetEntry {
    ir: bool,
    main: bool,
    camera: usize,
    seen_ms: u64,
}

#[derive(Default)]
struct Targets {
    by_fbo: HashMap<u32, TargetEntry>,
    /// G-buffer framebuffer id -> tonemap target rendered from it.
    gbuf_target: HashMap<u32, u32>,
    /// Tonemap target -> its G-buffer (the reverse lookup the composite needs).
    target_gbuf: HashMap<u32, u32>,
}

impl Targets {
    /// Drop targets no tonemap quad refreshed for `FLIR_TARGET_STALE_MS`, and the
    /// pairings that belong to them. A dropped feed target then has no pairing,
    /// so nothing is composited into it (nor substituted in its G-buffer) until
    /// its next quad re-learns it: a recycled framebuffer id is never trusted.
    fn prune(&mut self, now: u64) {
        let stale: Vec<u32> = self
            .by_fbo
            .iter()
            .filter(|(_, entry)| now.saturating_sub(entry.seen_ms) > FLIR_PAIRING_STALE_MS)
            .map(|(fbo, _)| *fbo)
            .collect();
        for target in stale {
            self.by_fbo.remove(&target);
            self.forget_pairing(target);
        }
    }

    fn forget_pairing(&mut self, target: u32) {
        if let Some(gbuffer) = self.target_gbuf.remove(&target) {
            if self.gbuf_target.get(&gbuffer) == Some(&target) {
                self.gbuf_target.remove(&gbuffer);
            }
        }
    }
}

static TARGETS: LazyLock<Mutex<Targets>> = LazyLock::new(|| Mutex::new(Targets::default()));
const TARGET_LIMIT: usize = 8;
/// A camera feed renders only every few seconds (the monitor refresh), and its
/// G-buffer pass runs BEFORE the tonemap quad that learns the pairing. The
/// pairing therefore has to outlive the gap between two feed renders (the
/// shared 1 s `FLIR_TARGET_STALE_MS` pruned it every time, so a feed's G-buffer
/// pass never saw a pairing and was never substituted). Whether the view is
/// still FLIR is re-checked live against the camera's enable latch (see
/// `update_active_view`), so a long lifetime cannot keep a disabled view hot.
const FLIR_PAIRING_STALE_MS: u64 = 30_000;
/// Draw framebuffer currently bound, in GL command order.
static CURRENT_FBO: AtomicU32 = AtomicU32::new(0);
/// Newest G-buffer-like framebuffer bound since the last tonemap quad: at the
/// quad that is the view's G-buffer.
static LAST_GBUF: AtomicU32 = AtomicU32::new(0);
/// The tonemap target that drew last, held until the first framebuffer bind after
/// its quad. That bind identifies the view: FBO 0 for the main view (FXAA
/// presents into the default framebuffer), another target for a camera feed.
static PENDING_QUAD_TARGET: AtomicU32 = AtomicU32::new(0);
/// IR decision of the last `0x68B8C0` call, for the tonemap quad that follows it
/// in GL order: 0 = no pending uniforms pass, 1 = pending and not IR-graded,
/// 2 = pending and IR-graded. Swapped to 0 by the quad draw, so only the real
/// tonemap quad learns a view - not the other passes that use the same shared
/// `0x68BB00` quad helper.
static TONEMAP_NEXT: AtomicU32 = AtomicU32::new(0);
/// FLIR-enabled camera of the pending tonemap quad (valid when `TONEMAP_NEXT == 2`).
static TONEMAP_CAMERA: AtomicUsize = AtomicUsize::new(0);
/// False when the 0x68B8C0 hook is not installed (marker or prologue mismatch):
/// every matched quad draw is then treated as a tonemap quad.
static TONEMAP_HOOK_OK: AtomicBool = AtomicBool::new(false);
/// Lock-free mirror of the G-buffers whose paired target is IR/F10-forced, so
/// the per-draw gate needs no lock (see `refresh_forced_gbuffers`).
const FORCED_GBUF_SLOTS: usize = 4;
static FORCED_GBUFS: [AtomicU32; FORCED_GBUF_SLOTS] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];
/// Per forced G-buffer slot: the FLIR camera whose enable latch must still be
/// live (0 = the F10 main view, which has no camera), and whether it is the main view.
static FORCED_CAMS: [AtomicUsize; FORCED_GBUF_SLOTS] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];
static FORCED_MAIN: [AtomicBool; FORCED_GBUF_SLOTS] = [
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
];
/// View kind the draw framebuffer currently bound belongs to for the block pass:
/// 0 = not substituted, 1 = main view (F10), 2 = camera feed with FLIR enabled.
/// Recomputed at every framebuffer bind, so the per-draw gate needs no lock.
static ACTIVE_VIEW: AtomicU32 = AtomicU32::new(0);
/// Mirror of "some target is currently IR-graded": gates matrix capture.
static IR_WANTED: AtomicBool = AtomicBool::new(false);

/// Bumped per scene submit: the body-volume upload throttle's clock. No rendering
/// decision uses it (see `TARGETS`).
static FRAME: AtomicU64 = AtomicU64::new(0);
static MAP_REBUILD_MS: AtomicU64 = AtomicU64::new(0);

/// One-shot keys for diagnostics that need a value-dependent key (view size,
/// framebuffer id), unlike `logging::log_once`'s fixed bits. Capped so a game
/// that cycles targets cannot grow it without bound.
static DIAG_KEYS: Mutex<Vec<u64>> = Mutex::new(Vec::new());
const DIAG_KEY_LIMIT: usize = 24;

fn log_once_key(key: u64, message: impl FnOnce() -> String) {
    #[cfg(feature = "logging")]
    {
        let mut keys = DIAG_KEYS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // 0x6a5_xxxx: a handful of fixed feature keys, never starved by the
        // per-object keys filling the cap.
        if (keys.len() >= DIAG_KEY_LIMIT && key >> 16 != 0x6a5) || keys.contains(&key) {
            return;
        }
        keys.push(key);
        drop(keys);
        log_line(&message());
    }
    #[cfg(not(feature = "logging"))]
    let _ = (key, message);
}
static F10_ON: AtomicBool = AtomicBool::new(false);
static KEY_HELD: AtomicBool = AtomicBool::new(false);

/// Cameras whose IR latch was seen set, newest tick wins. Each one keeps the
/// ambient temperature last sampled at its own pivot, so a view reads its own
/// camera's ambient instead of whichever camera ticked last.
const CAMERA_SLOTS: usize = 4;

struct CameraSlot {
    pointer: AtomicUsize,
    ambient_bits: AtomicU32,
    ambient_valid: AtomicBool,
    tick_ms: AtomicU64,
}

impl CameraSlot {
    const fn new() -> CameraSlot {
        CameraSlot {
            pointer: AtomicUsize::new(0),
            ambient_bits: AtomicU32::new(0),
            ambient_valid: AtomicBool::new(false),
            tick_ms: AtomicU64::new(0),
        }
    }
}

static CAMERAS: [CameraSlot; CAMERA_SLOTS] = [
    CameraSlot::new(),
    CameraSlot::new(),
    CameraSlot::new(),
    CameraSlot::new(),
];

// One-shot diagnostic keys (verbose builds only, see `logging::log_once`).
// `flir` owns bits 8..32 of the shared one-shot word; `blocktemp` owns 0..8.
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
/// at ambient). Only counted/reported in logging builds.
#[cfg(feature = "logging")]
static DIAG_COUNTS: [[AtomicU32; 4]; 2] = [
    [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)],
    [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)],
];
#[cfg(feature = "logging")]
static DIAG_LAST_MS: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "logging")]
fn diag_count(which: usize) {
    let view = ACTIVE_VIEW.load(Ordering::Relaxed);
    if view != 0 {
        DIAG_COUNTS[(view - 1) as usize][which].fetch_add(1, Ordering::Relaxed);
    }
}
#[cfg(not(feature = "logging"))]
fn diag_count(_which: usize) {}

/// One line per ~5 s while any view is FLIR-active (called at every tonemap quad).
#[cfg(feature = "logging")]
fn diag_tick(now: u64) {
    if !IR_WANTED.load(Ordering::Relaxed) && !(cfg!(debug_assertions) && F10_ON.load(Ordering::Relaxed)) {
        return;
    }
    let last = DIAG_LAST_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < 5000 {
        return;
    }
    DIAG_LAST_MS.store(now, Ordering::Relaxed);
    if last == 0 {
        return;
    }
    let secs = (now - last) as f64 / 1000.0;
    let take = |view: usize| -> [u32; 4] {
        [0, 1, 2, 3].map(|i| DIAG_COUNTS[view][i].swap(0, Ordering::Relaxed))
    };
    let (main, feed) = (take(0), take(1));
    let entries = MESH_BODY.lock().unwrap_or_else(|p| p.into_inner()).len();
    let rate = |n: u32| n as f64 / secs;
    log_debug(|| {
        format!(
            "FLIR counters per s over {secs:.1} s (map {entries} ibos): \
main substituted={:.1} unmapped_ibo={:.1} no_grid_yet={:.1} component={:.1} | \
feed substituted={:.1} unmapped_ibo={:.1} no_grid_yet={:.1} component={:.1}",
            rate(main[0]), rate(main[1]), rate(main[2]), rate(main[3]),
            rate(feed[0]), rate(feed[1]), rate(feed[2]), rate(feed[3])
        )
    });
    let queries = GL_QUERIES.swap(0, Ordering::Relaxed);
    let hook_us = HOOK_US.swap(0, Ordering::Relaxed);
    let sub_us = HOOK_SUB_US.swap(0, Ordering::Relaxed);
    log_debug(|| {
        format!(
            "FLIR perf per s: gl_queries={:.1} draw_thunk_own_ms={:.2} (substituted draws {:.2}) top query sites (src/flir.rs line, count): {:?}",
            queries as f64 / secs,
            hook_us as f64 / 1000.0 / secs,
            sub_us as f64 / 1000.0 / secs,
            {
                let mut sites = std::mem::take(&mut *GLQ_SITES.lock().unwrap_or_else(|p| p.into_inner()));
                sites.sort_by(|a, b| b.1.cmp(&a.1));
                sites.truncate(6);
                sites
            },
        )
    });
    for (label, row) in [("map-hit", 1usize), ("component/unmapped", 0)] {
        let parts: Vec<String> = (0..SK_N)
            .map(|i| format!("{}={:.1}", SK_NAMES[i], rate(SKIP_COUNTS[row][i].swap(0, Ordering::Relaxed))))
            .collect();
        log_debug(|| format!("FLIR counters main-view per s, {label} ibos: {}", parts.join(" ")));
    }
}
#[cfg(not(feature = "logging"))]
fn diag_tick(_now: u64) {}

/// Diagnostic that is not one-shot (a state transition). Verbose builds only:
/// wiring lines stay out of the player build, which logs only prologue and
/// hook-install failures.
fn log_debug(message: impl FnOnce() -> String) {
    #[cfg(feature = "logging")]
    log_line(&message());
    #[cfg(not(feature = "logging"))]
    let _ = message;
}

// ---------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------

/// Marker file next to the DLL (`<plugin>/infrared_disable_*`). The log lives in
/// `<plugin>/logs/`, so the directory is its grandparent.
fn marker_exists(name: &str) -> bool {
    let path = crate::state::LOG_PATH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let Some(log_path) = path else { return false };
    let Some(logs) = log_path.parent() else { return false };
    let Some(plugin_dir) = logs.parent() else { return false };
    plugin_dir.join(name).exists()
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
pub(crate) unsafe fn install(base: usize) {
    if FLIR_DISABLED.load(Ordering::SeqCst) {
        log_line("FLIR hooks disabled by request");
        return;
    }
    let tonemap_off = marker_exists("infrared_disable_flir_tonemap");
    DRAW_THUNK_DISABLED.store(marker_exists("infrared_disable_flir_draw"), Ordering::SeqCst);
    MATRIX_THUNK_DISABLED.store(
        marker_exists("infrared_disable_flir_matrix"),
        Ordering::SeqCst,
    );
    FBO_THUNK_DISABLED.store(marker_exists("infrared_disable_flir_fbo"), Ordering::SeqCst);
    let (draw_off, matrix_off, fbo_off) = (
        DRAW_THUNK_DISABLED.load(Ordering::SeqCst),
        MATRIX_THUNK_DISABLED.load(Ordering::SeqCst),
        FBO_THUNK_DISABLED.load(Ordering::SeqCst),
    );
    if tonemap_off || draw_off || matrix_off || fbo_off {
        log_line(&format!(
            "FLIR per-hook switches: tonemap={} draw={} matrix={} fbo={} \
(the disabled hook(s) are NOT installed)",
            tonemap_off as u8, draw_off as u8, matrix_off as u8, fbo_off as u8
        ));
    }
    if !tonemap_off {
        install_code_hook(
            base,
            RVA_TONEMAP,
            &TONEMAP_PROLOGUE,
            "tonemap pass",
            ir_tonemap_hook as *const c_void,
            &ORIG_TONEMAP,
        );
        // The quad hint is only meaningful when the detour is installed; the
        // draw thunk falls back to "every matched quad" without it.
        TONEMAP_HOOK_OK.store(ORIG_TONEMAP.load(Ordering::SeqCst) != 0, Ordering::SeqCst);
    }

    GL_BASE.store(base, Ordering::SeqCst);
    ensure_gl_slots();
}

/// (Re-)point the GL dispatch slots at our thunks. Safe to call twice: a slot
/// that already forwards to us has a non-zero original and is skipped.
unsafe fn ensure_gl_slots() {
    let base = GL_BASE.load(Ordering::SeqCst);
    if base == 0 {
        return;
    }
    let slots = [
        (
            RVA_GL_DRAW_ELEMENTS,
            swi_gl_draw_elements_entry as *const () as usize,
            &ORIG_GL_DRAW_ELEMENTS,
            "glDrawElements",
            DRAW_THUNK_DISABLED.load(Ordering::SeqCst),
        ),
        (
            RVA_GL_UNIFORM_MATRIX4FV,
            gl_uniform_matrix4fv_hook as *const () as usize,
            &ORIG_GL_UNIFORM_MATRIX4FV,
            "glUniformMatrix4fv",
            MATRIX_THUNK_DISABLED.load(Ordering::SeqCst),
        ),
        (
            RVA_GL_BIND_FRAMEBUFFER,
            gl_bind_framebuffer_hook as *const () as usize,
            &ORIG_GL_BIND_FRAMEBUFFER,
            "glBindFramebuffer",
            FBO_THUNK_DISABLED.load(Ordering::SeqCst),
        ),
        (
            RVA_GL_USE_PROGRAM,
            gl_use_program_hook as *const () as usize,
            &ORIG_GL_USE_PROGRAM,
            "glUseProgram",
            false,
        ),
        (
            RVA_GL_BIND_BUFFER,
            gl_bind_buffer_hook as *const () as usize,
            &ORIG_GL_BIND_BUFFER,
            "glBindBuffer",
            false,
        ),
        (
            RVA_GL_ACTIVE_TEXTURE,
            gl_active_texture_hook as *const () as usize,
            &ORIG_GL_ACTIVE_TEXTURE,
            "glActiveTexture",
            false,
        ),
        (
            RVA_GL_BIND_VERTEX_ARRAY,
            gl_bind_vertex_array_hook as *const () as usize,
            &ORIG_GL_BIND_VERTEX_ARRAY,
            "glBindVertexArray",
            false,
        ),
        (
            RVA_GL_UNIFORM1F,
            gl_uniform1f_hook as *const () as usize,
            &ORIG_GL_UNIFORM1F,
            "glUniform1f",
            false,
        ),
        (
            RVA_GL_UNIFORM3F,
            gl_uniform3f_hook as *const () as usize,
            &ORIG_GL_UNIFORM3F,
            "glUniform3f",
            false,
        ),
        (
            RVA_GL_BIND_TEXTURE,
            gl_bind_texture_hook as *const () as usize,
            &ORIG_GL_BIND_TEXTURE,
            "glBindTexture",
            false,
        ),
        (
            RVA_GL_VIEWPORT,
            gl_viewport_hook as *const () as usize,
            &ORIG_GL_VIEWPORT,
            "glViewport",
            false,
        ),
    ];
    for (rva, ours, original, label, disabled) in slots {
        if disabled {
            continue;
        }
        if original.load(Ordering::SeqCst) == 0 {
            patch_gl_slot(base + rva, ours, original, label);
        }
    }
}


unsafe fn install_code_hook(
    base: usize,
    rva: usize,
    prologue: &[u8],
    label: &str,
    detour: *const c_void,
    original: &AtomicUsize,
) {
    let address = base + rva;
    if !bytes_match(address, prologue) {
        log_line(&format!(
            "FLIR {label} prologue mismatch at {address:#x}; FLIR {label} hook skipped (this build is not {GAME_BUILD_LABEL})"
        ));
        return;
    }
    match minhook::MinHook::create_hook(address as *mut c_void, detour as *mut c_void) {
        Ok(trampoline) => {
            original.store(trampoline as usize, Ordering::SeqCst);
            if let Err(status) = minhook::MinHook::enable_hook(address as *mut c_void) {
                log_line(&format!("FLIR: MinHook enable_hook({label}) failed: {status}"));
            } else {
                log_debug(|| format!("FLIR {label} hook installed at {address:#x}"));
            }
        }
        Err(status) => log_line(&format!(
            "FLIR: MinHook create_hook({label}) failed: {status}"
        )),
    }
}

/// Re-point a GL dispatch slot at `ours`, keeping the previous pointer as the
/// forwarding target. Both known slots (`0x140A935F8` IAT entry,
/// `0x140CE3268`/`0x140CE5688` GLEW globals) are plain pointer variables, so the
/// slot itself is the target. Every read is validated and the value is only
/// accepted when it points into a loaded module, so a wrong RVA or an
/// uninitialised slot cannot be published as the forwarding target.
unsafe fn patch_gl_slot(slot: usize, ours: usize, original: &AtomicUsize, label: &str) {
    let target = slot;
    if !readable(target, std::mem::size_of::<usize>()) {
        log_debug(|| {
            format!(
                "FLIR: GL slot {label} target {target:#x} is not mapped; {label} hook skipped"
            )
        });
        return;
    }
    let previous = *(target as *const usize);
    if previous == ours {
        return;
    }
    if !image_backed(previous) {
        log_debug(|| {
            format!(
                "FLIR: GL slot {label} at {slot:#x} does not hold a module pointer (value {previous:#x}); {label} hook skipped"
            )
        });
        return;
    }
    let mut old_protect: PAGE_PROTECTION_FLAGS = 0;
    if VirtualProtect(
        target as *const c_void,
        std::mem::size_of::<usize>(),
        PAGE_EXECUTE_READWRITE,
        &mut old_protect,
    ) == 0
    {
        log_debug(|| {
            format!(
                "FLIR: could not unprotect GL slot {label} at {target:#x}; {label} hook skipped"
            )
        });
        return;
    }
    original.store(previous, Ordering::SeqCst);
    *(target as *mut usize) = ours;
    let mut ignored: PAGE_PROTECTION_FLAGS = 0;
    let _ = VirtualProtect(
        target as *const c_void,
        std::mem::size_of::<usize>(),
        old_protect,
        &mut ignored,
    );
    log_debug(|| {
        format!("FLIR: GL slot {label} hooked at {target:#x} (was {previous:#x})")
    });
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

/// Record `camera` in the slot registry: its own slot, else an empty one, else
/// the one that has been stale longest.
fn register_camera(camera: usize, now: u64) {
    let mut candidate: Option<usize> = None;
    let mut candidate_ms = u64::MAX;
    for (index, slot) in CAMERAS.iter().enumerate() {
        let pointer = slot.pointer.load(Ordering::Relaxed);
        if pointer == camera {
            slot.tick_ms.store(now, Ordering::Relaxed);
            return;
        }
        if pointer < 0x10000 {
            candidate = Some(index);
            break;
        }
        let tick = slot.tick_ms.load(Ordering::Relaxed);
        if tick < candidate_ms {
            candidate_ms = tick;
            candidate = Some(index);
        }
    }
    let Some(index) = candidate else { return };
    let slot = &CAMERAS[index];
    slot.pointer.store(camera, Ordering::Relaxed);
    slot.tick_ms.store(now, Ordering::Relaxed);
    slot.ambient_valid.store(false, Ordering::Relaxed);
}

/// Ambient temperature last sampled at `camera`'s own pivot.
fn camera_ambient(camera: usize) -> f32 {
    for slot in CAMERAS.iter() {
        if slot.pointer.load(Ordering::Relaxed) == camera
            && slot.ambient_valid.load(Ordering::Relaxed)
        {
            return f32::from_bits(slot.ambient_bits.load(Ordering::Relaxed));
        }
    }
    FLIR_AMBIENT_FALLBACK_C
}

/// Scene ambient air temperature cached by `scene_tick` on the game thread (the
/// median of the thermal sim's per-body ambient samples), or the fixed fallback
/// when the sim has not stepped a body yet. Used by the main view (F10), which has
/// no IR camera to sample at.
fn scene_ambient() -> f32 {
    let value = f32::from_bits(FLIR_SCENE_AMBIENT_BITS.load(Ordering::Relaxed));
    if value.is_finite() {
        value
    } else {
        FLIR_AMBIENT_FALLBACK_C
    }
}

// Camera/view discovery at the scene submit `0x6BB400`. The entry stub saves the
// caller's RBX/R15/RSI (callee-saved, still intact at function entry) and jumps to
// the Rust detour with every argument register and the stack untouched. Per the
// RE: at the main submit RSI == arg11 (main view state) and R15 = main camera; at
// the feed submit RBX = the feed camera and arg11 = the feed view state.
static SUBMIT_RBX: AtomicUsize = AtomicUsize::new(0);
static SUBMIT_R15: AtomicUsize = AtomicUsize::new(0);
static SUBMIT_RSI: AtomicUsize = AtomicUsize::new(0);
core::arch::global_asm!(
    ".globl swi_scene_submit_entry",
    "swi_scene_submit_entry:",
    "mov qword ptr [rip + {rbx}], rbx",
    "mov qword ptr [rip + {r15}], r15",
    "mov qword ptr [rip + {rsi}], rsi",
    "jmp {body}",
    rbx = sym SUBMIT_RBX,
    r15 = sym SUBMIT_R15,
    rsi = sym SUBMIT_RSI,
    body = sym crate::hooks::ir_frame_uniforms_hook,
);
extern "C" {
    fn swi_scene_submit_entry();
}
/// Address of the scene-submit entry stub (the MinHook detour for `0x6BB400`).
pub(crate) fn scene_submit_entry() -> usize {
    swi_scene_submit_entry as *const () as usize
}

/// Main view's camera (R15 at the main submit), 0 until seen.
static MAIN_CAMERA: AtomicUsize = AtomicUsize::new(0);
/// Persistent render-target holder W -> (camera, last seen ms). Never keyed by the
/// feed view state, which is `operator new`'d and freed every frame.
static HOLDER_CAMERA: LazyLock<Mutex<HashMap<usize, (usize, u64)>>> =
    LazyLock::new(|| Mutex::new(HashMap::default()));
const HOLDER_STALE_MS: u64 = 5000;
const HOLDER_LIMIT: usize = 64;

fn valid_camera(camera: usize) -> bool {
    let base = GL_BASE.load(Ordering::Relaxed);
    base != 0
        && camera >= PTR_MIN
        && camera % 8 == 0 // register values are arbitrary integers; objects are 8-aligned
        && readable(camera, 8)
        && unsafe { read_usize_at(camera) } == base + RVA_CAMERA_VTABLE
}

/// Each scene submit (0x6bb400) renders one view and tells us which camera it is
/// for. Also the frame clock (body-volume upload throttle). Block substitution
/// and composite are still gated by GL objects/order, never by submit order.
pub(crate) fn on_scene_submit(_scene: usize, view_state: usize, secondary: u8) {
    // A panic must never unwind through the `extern "C"` submit detour.
    let _ = std::panic::catch_unwind(|| {
        FRAME.fetch_add(1, Ordering::Relaxed);
        if secondary != 0 {
            log_once(LOG_FLIR_SUBMIT, || {
                "FLIR: editor/preview scene submit observed (arg 12 = 1)".to_string()
            });
        }
        let (rbx, r15, rsi) = (
            SUBMIT_RBX.load(Ordering::Relaxed),
            SUBMIT_R15.load(Ordering::Relaxed),
            SUBMIT_RSI.load(Ordering::Relaxed),
        );
        if view_state < PTR_MIN {
            return;
        }
        if rsi == view_state {
            if valid_camera(r15) {
                MAIN_CAMERA.store(r15, Ordering::Relaxed);
            }
            return;
        }
        if !valid_camera(rbx) || !readable(view_state + VIEW_STATE_RT_HOLDER, 8) {
            return;
        }
        let holder = unsafe { read_usize_at(view_state + VIEW_STATE_RT_HOLDER) };
        if holder < PTR_MIN {
            return;
        }
        let now = now_ms();
        let mut map = HOLDER_CAMERA.lock().unwrap_or_else(|p| p.into_inner());
        map.retain(|_, (_, seen)| now.saturating_sub(*seen) <= HOLDER_STALE_MS);
        if map.len() >= HOLDER_LIMIT && !map.contains_key(&holder) {
            map.clear();
        }
        map.insert(holder, (rbx, now));
    });
}

/// Camera of the view whose tonemap params block is `params` (`view_state+0x870`):
/// null holder = main view (its submit camera), else the holder's mapped camera.
fn camera_for_params(params: usize) -> usize {
    let Some(view_state) = params.checked_sub(VIEW_STATE_TONEMAP_PARAMS) else {
        return 0;
    };
    if view_state < PTR_MIN || !readable(view_state + VIEW_STATE_RT_HOLDER, 8) {
        return 0;
    }
    let holder = unsafe { read_usize_at(view_state + VIEW_STATE_RT_HOLDER) };
    if holder == 0 {
        return MAIN_CAMERA.load(Ordering::Relaxed);
    }
    let now = now_ms();
    HOLDER_CAMERA
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&holder)
        .filter(|(_, seen)| now.saturating_sub(*seen) <= HOLDER_STALE_MS)
        .map_or(0, |(camera, _)| *camera)
}

/// Scene tick (game thread): the FLIR mesh -> body map, plus the ambient
/// temperature at each registered camera's own pivot, so the render thread
/// never calls into the heat model itself and every view can use its own
/// camera's ambient.
pub(crate) fn scene_tick(scene: usize) {
    // A panic must never unwind through the `extern "C"` scene-tick detour.
    let _ = std::panic::catch_unwind(|| scene_tick_inner(scene));
}

fn scene_tick_inner(scene: usize) {
    // Mesh index buffer -> body map for the FLIR block pass. The walk reads the
    // game's vehicle/body containers, and ONLY the game thread may do that: a
    // vehicle spawn rebuilds them while the render thread draws, which is a data
    // race (it crashed the game). Published under `MESH_BODY`; the render thread
    // only ever looks an index buffer up in the published map.
    if scene >= PTR_MIN {
        let now = now_ms();
        let since = now.saturating_sub(MAP_REBUILD_MS.load(Ordering::Relaxed));
        // Only while some view can use it (a camera with ir.flir on, or F10):
        // otherwise the walk is pure overhead. The first tick after FLIR turns
        // on rebuilds immediately because the throttle has long expired.
        let wanted = IR_WANTED.load(Ordering::Relaxed)
            || (cfg!(debug_assertions) && F10_ON.load(Ordering::Relaxed));
        if wanted && since > 250 {
            MAP_REBUILD_MS.store(now, Ordering::Relaxed);
            let _stage = fault::enter(stage::FLIR_MESH_MAP);
            rebuild_mesh_body(scene);
        }
    }
    let now = now_ms();
    for slot in CAMERAS.iter() {
        let camera = slot.pointer.load(Ordering::Relaxed);
        if camera < 0x10000
            || now.saturating_sub(slot.tick_ms.load(Ordering::Relaxed)) > FLIR_IR_CAMERA_STALE_MS
        {
            continue;
        }
        if !readable(camera + CAMERA_PIVOT, 24) {
            continue;
        }
        let (x, y, z) = unsafe {
            (
                read_f64_at(camera + CAMERA_PIVOT),
                read_f64_at(camera + CAMERA_PIVOT + 8),
                read_f64_at(camera + CAMERA_PIVOT + 16),
            )
        };
        if !(x.is_finite() && y.is_finite() && z.is_finite()) {
            continue;
        }
        let ambient = unsafe { crate::heat::base_ambient_at((x, y, z)) };
        if ambient.is_finite() {
            slot.ambient_bits
                .store(ambient.to_bits(), Ordering::Relaxed);
            slot.ambient_valid.store(true, Ordering::Relaxed);
        }
    }
    // A view with no IR camera of its own (the F10 main view) uses the scene's
    // ambient temperature. The heat evaluator is game-thread only, so cache a
    // value here: the median of the per-body ambient samples the sim just took.
    // The render thread only reads the atomic.
    if let Some(ambient) = crate::blocktemp::median_body_ambient() {
        FLIR_SCENE_AMBIENT_BITS.store(ambient.to_bits(), Ordering::Relaxed);
    }
    if let Some(ground) = crate::blocktemp::median_ground_ambient() {
        GROUND_AMBIENT_BITS.store(ground.to_bits(), Ordering::Relaxed);
    }
    // Sun, time of day and haze for the composite (game-thread reads, cached
    // for the render thread).
    if let Some((to_sun, percent)) = unsafe { crate::sun::game_sun() } {
        *GAME_SUN.lock().unwrap_or_else(|p| p.into_inner()) = Some([to_sun[0] as f32, to_sun[1] as f32, to_sun[2] as f32]);
        DAY_PCT_BITS.store((percent as f32).to_bits(), Ordering::Relaxed);
    }
    let (fog, rain) = unsafe { crate::ir::weather_fog_rain() };
    // FlightGear scales its diurnal surface deltas by the clear-sky factor.
    CLEAR_SKY_BITS.store((1.0 - 0.8 * fog.max(rain)).clamp(0.2, 1.0).to_bits(), Ordering::Relaxed);
}

/// Jet exhaust plumes per body (body-local metres): start, dir, radius, excess C.
type Plume = ([f32; 3], [f32; 3], f32, f32);
static PLUMES: Mutex<Vec<(usize, u64, Vec<Plume>)>> = Mutex::new(Vec::new());
const MAX_PLUMES: usize = 8;

/// Game thread (blocktemp body step): replace a body's plumes.
pub(crate) fn set_body_plumes(body: usize, plumes: Vec<Plume>) {
    let now = crate::state::now_ms();
    let mut all = PLUMES.lock().unwrap_or_else(|p| p.into_inner());
    all.retain(|e| e.0 != body && now.saturating_sub(e.1) < 2_000);
    if !plumes.is_empty() && all.len() < 64 {
        all.push((body, now, plumes));
    }
}

/// Render thread: this view's plumes in render space, packed for the
/// composite: a = (start, excess), b = (dir, radius).
fn view_plumes() -> (Vec<[f32; 4]>, Vec<[f32; 4]>) {
    let (mut a, mut b) = (Vec::new(), Vec::new());
    let all = PLUMES.lock().unwrap_or_else(|p| p.into_inner());
    if all.is_empty() {
        return (a, b);
    }
    let view = ACTIVE_VIEW.load(Ordering::Relaxed);
    let mats = BODY_MATS.lock().unwrap_or_else(|p| p.into_inner());
    for (body, _, plumes) in all.iter() {
        let Some(m) = mats.iter().filter(|e| e.view == view && e.body == *body).max_by_key(|e| e.frame).map(|e| e.mat) else {
            continue;
        };
        for &(p, d, r, excess) in plumes {
            if a.len() >= MAX_PLUMES {
                break;
            }
            let w = [
                m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12],
                m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13],
                m[2] * p[0] + m[6] * p[1] + m[10] * p[2] + m[14],
            ];
            let wd = [
                m[0] * d[0] + m[4] * d[1] + m[8] * d[2],
                m[1] * d[0] + m[5] * d[1] + m[9] * d[2],
                m[2] * d[0] + m[6] * d[1] + m[10] * d[2],
            ];
            let n = (wd[0] * wd[0] + wd[1] * wd[1] + wd[2] * wd[2]).sqrt();
            if !(n > 1e-6) || !w.iter().all(|v| v.is_finite()) {
                continue;
            }
            a.push([w[0], w[1], w[2], excess]);
            b.push([wd[0] / n, wd[1] / n, wd[2] / n, r]);
        }
    }
    (a, b)
}

/// Sea-level air under the vehicles: terrain/water ambient (NaN until known).
static GROUND_AMBIENT_BITS: AtomicU32 = AtomicU32::new(0x7fc0_0000);
/// To-sun unit vector from the game's clock-driven sun (scene tick).
static GAME_SUN: Mutex<Option<[f32; 3]>> = Mutex::new(None);
/// Day fraction (0 = midnight, 0.5 = noon) and clear-sky factor (0.2..1).
static DAY_PCT_BITS: AtomicU32 = AtomicU32::new(0x3f00_0000);
static CLEAR_SKY_BITS: AtomicU32 = AtomicU32::new(0x3f80_0000);

/// Tonemap uniforms `0x68b8c0` (verified: uniforms only, no draw). `params` is
/// `view_state+0x870` (`r8`). It runs immediately before the view's tonemap quad
/// in GL order: it resolves the view's camera and, when `ir.flir(true)` is fresh for
/// that camera, marks the next `0x68BB00` quad for the FLIR composite.
pub(crate) extern "C" fn ir_tonemap_hook(ctx: usize, view: usize, params: usize) {
    let original = ORIG_TONEMAP.load(Ordering::SeqCst);
    // A panic must never unwind through the `extern "C"` detour.
    let camera = std::panic::catch_unwind(|| {
        let camera = camera_for_params(params);
        if camera >= PTR_MIN && flir_enabled_cfg(camera).is_some() {
            register_camera(camera, now_ms());
            camera
        } else {
            0
        }
    })
    .unwrap_or(0);
    // Publish before forwarding: the quad draw follows on this thread immediately.
    TONEMAP_CAMERA.store(camera, Ordering::Relaxed);
    TONEMAP_NEXT.store(if camera != 0 { 2 } else { 1 }, Ordering::Release);
    if original != 0 {
        let f: extern "C" fn(usize, usize, usize) = unsafe { std::mem::transmute(original) };
        f(ctx, view, params);
    }
    if camera != 0 {
        log_once(LOG_FLIR_TONEMAP, || {
            format!("FLIR: tonemap pass for FLIR-enabled camera {camera:#x}; the quad that follows is composited")
        });
    }
}

// ---------------------------------------------------------------------------
// Target registry (view identity by GL object)
// ---------------------------------------------------------------------------

/// Recompute the lock-free mirrors from `targets` (caller holds the lock).
fn publish_targets(targets: &Targets, f10: bool) {
    IR_WANTED.store(targets.by_fbo.values().any(|entry| entry.ir), Ordering::Relaxed);
    let mut forced: Vec<(u32, usize, bool)> = Vec::new();
    for (gbuffer, target) in targets.gbuf_target.iter() {
        // Only a G-buffer paired with a target whose last tonemap quad was
        // IR-graded (or, in a debug build, the main view while F10 is on) is
        // substituted in. A feed target is never forced by F10; an unpaired
        // G-buffer is never touched at all.
        let forced_entry = match targets.by_fbo.get(target) {
            Some(entry) if entry.ir => Some((entry.camera, entry.main)),
            Some(entry) if f10 && entry.main => Some((0, true)),
            _ => None,
        };
        if let Some((camera, main)) = forced_entry {
            log_once_key(0x06a5_e000 | ((*gbuffer as u64 & 0x7f) << 7) | (*target as u64 & 0x7f), || {
                format!(
                    "FLIR: G-buffer {gbuffer} of target {target} now substituted (main={main}, camera={camera:#x})"
                )
            });
            forced.push((*gbuffer, camera, main));
        }
    }
    for (index, slot) in FORCED_GBUFS.iter().enumerate() {
        let (gbuffer, camera, main) = forced.get(index).copied().unwrap_or((0, 0, false));
        FORCED_CAMS[index].store(camera, Ordering::Relaxed);
        FORCED_MAIN[index].store(main, Ordering::Relaxed);
        slot.store(gbuffer, Ordering::Relaxed);
    }
}

/// Decide, for the draw framebuffer that was just bound, whether the block pass
/// substitutes into it: it must be a paired G-buffer of an IR-graded target whose
/// camera's FLIR latch is STILL on (checked here, once per bind, not per draw), or
/// the F10 main view. A feed is judged by its own camera, never by the main view.
fn update_active_view(framebuffer: u32) {
    let mut kind = 0;
    if framebuffer != 0 {
        for (index, slot) in FORCED_GBUFS.iter().enumerate() {
            if slot.load(Ordering::Relaxed) != framebuffer {
                continue;
            }
            let camera = FORCED_CAMS[index].load(Ordering::Relaxed);
            if camera == 0 || flir_enabled_cfg(camera).is_some() {
                kind = if FORCED_MAIN[index].load(Ordering::Relaxed) { 1 } else { 2 };
            }
            break;
        }
    }
    ACTIVE_VIEW.store(kind, Ordering::Relaxed);
}

/// Refresh the mirrors against the live registry (F10 can change at any time).
fn refresh_forced_gbuffers() {
    let f10 = cfg!(debug_assertions) && f10_toggled();
    let now = now_ms();
    let mut targets = TARGETS.lock().unwrap_or_else(|p| p.into_inner());
    targets.prune(now);
    publish_targets(&targets, f10);
    drop(targets);
    update_active_view(CURRENT_FBO.load(Ordering::Relaxed));
}

/// Learn what a tonemap quad says about its view: the target's own IR state (the
/// `0x68B8C0` params of that quad) and, when the G-buffer bound before it is a real
/// one, the G-buffer/target pairing. This is the only place a view's G-buffer is
/// identified: GL order is the authority, not the submit order. Returns whether the
/// target is already known as the main view. One log line per learned pairing.
/// Set by the game thread when the scene object changes (world load/unload).
pub(crate) static RENDER_TARGETS_STALE: AtomicBool = AtomicBool::new(false);

/// Drop the probed G-buffer descriptions (positive and negative). GL recycles
/// framebuffer/texture ids when the game rebuilds its render targets (menu ->
/// world -> menu), so a cached id can describe a deleted target: the in-world
/// view then samples the menu's textures (flat grey) or a new G-buffer whose id
/// was once rejected is never captured. Rebuilt by the next binds.
fn forget_gbuffer_probes(reason: &str) {
    GBUFFERS.lock().unwrap_or_else(|p| p.into_inner()).clear();
    NOT_GBUFFERS.lock().unwrap_or_else(|p| p.into_inner()).clear();
    log_debug(|| format!("FLIR: G-buffer probes dropped ({reason})"));
}

fn note_quad(gbuffer: u32, target: u32, ir: bool, camera: usize, now: u64) -> bool {
    if RENDER_TARGETS_STALE.swap(false, Ordering::Relaxed) {
        forget_gbuffer_probes("scene changed");
    }
    let f10 = cfg!(debug_assertions) && f10_toggled();
    let mut new_target = false;
    let mut targets = TARGETS.lock().unwrap_or_else(|p| p.into_inner());
    targets.prune(now);
    if targets.by_fbo.len() >= TARGET_LIMIT && !targets.by_fbo.contains_key(&target) {
        // A long session can cycle targets; keep the registry small rather than
        // letting it grow (a cleared target is relearned from its next quad).
        targets.by_fbo.clear();
        targets.gbuf_target.clear();
        targets.target_gbuf.clear();
    }
    match targets.by_fbo.get_mut(&target) {
        Some(entry) => {
            let was_ir = entry.ir;
            entry.ir = ir;
            entry.seen_ms = now;
            if ir && camera >= PTR_MIN {
                entry.camera = camera;
            } else if !ir {
                entry.camera = 0;
            }
            if target == 0 {
                // A quad into the default framebuffer IS the main view; no bind
                // follows it to learn from.
                entry.main = true;
            }
            if was_ir != ir {
                log_debug(|| {
                    format!("FLIR: tonemap target {target} is now IR-graded={}", ir as u8)
                });
            }
        }
        None => {
            new_target = true;
            targets.by_fbo.insert(
                target,
                TargetEntry {
                    ir,
                    main: target == 0,
                    camera: if ir { camera } else { 0 },
                    seen_ms: now,
                },
            );
            log_debug(|| {
                format!(
                    "FLIR: tonemap target {target} learned at its tonemap quad (IR={})",
                    ir as u8
                )
            });
        }
    }

    let mut previous = None;
    if gbuffer != 0 && gbuffer != target {
        if targets.gbuf_target.len() >= TARGET_LIMIT
            && !targets.gbuf_target.contains_key(&gbuffer)
            && !targets.target_gbuf.contains_key(&target)
        {
            // A game that cycles render targets must not grow the pairing either;
            // both sides are relearned at the next quad.
            targets.gbuf_target.clear();
            targets.target_gbuf.clear();
        }
        previous = targets.gbuf_target.insert(gbuffer, target);
        // A target rendered from another G-buffer now: drop the old pairing, so a
        // stale G-buffer id can never claim the target (or be substituted in).
        if let Some(old_gbuffer) = targets.target_gbuf.insert(target, gbuffer) {
            if old_gbuffer != gbuffer && targets.gbuf_target.get(&old_gbuffer) == Some(&target) {
                targets.gbuf_target.remove(&old_gbuffer);
            }
        }
        if let Some(old_target) = previous {
            if old_target != target && targets.target_gbuf.get(&old_target) == Some(&gbuffer) {
                targets.target_gbuf.remove(&old_target);
            }
        }
    }
    let main = targets.by_fbo.get(&target).is_some_and(|entry| entry.main);
    publish_targets(&targets, f10);
    drop(targets);
    if new_target {
        // A new tonemap target means the game (re)built its render targets.
        forget_gbuffer_probes("new tonemap target");
    }
    if gbuffer == 0 || gbuffer == target || previous == Some(target) {
        return main;
    }
    // Keyed by the pair, so a target that alternates between G-buffers (or a
    // recycled id) cannot turn this into a per-frame line.
    let key = 0x06a5_c000 | ((gbuffer as u64 & 0x7f) << 7) | (target as u64 & 0x7f);
    log_once_key(key, || {
        format!(
            "FLIR: G-buffer {gbuffer} -> tonemap target {target} ({} view, IR-graded={}){}",
            if main { "main" } else { "feed/other" },
            ir as u8,
            if previous.is_some() { ", changed" } else { "" }
        )
    });
    main
}

/// The first framebuffer bind after a tonemap quad identifies that view: the main
/// view presents into the default framebuffer (FBO 0, where FXAA draws), a camera
/// feed renders into an offscreen target. Learned per tonemap target; F10 only ever
/// forces a target learned as the main view. One log line per target/state.
fn note_first_bind(target: u32, framebuffer: u32) {
    let f10 = cfg!(debug_assertions) && f10_toggled();
    let now = now_ms();
    let mut targets = TARGETS.lock().unwrap_or_else(|p| p.into_inner());
    targets.prune(now);
    let Some(entry) = targets.by_fbo.get_mut(&target) else {
        return;
    };
    let main = framebuffer == 0;
    if main && !entry.main {
        entry.main = true;
        publish_targets(&targets, f10);
    }
    drop(targets);
    let key = (7u64 << 56) | (((target as u64) & 0xffff) << 8) | main as u64;
    log_once_key(key, || {
        format!(
            "FLIR: tonemap target {target}: first bind after its quad is FBO {framebuffer} ({})",
            if main {
                "main view, F10 may force it"
            } else {
                "offscreen view (camera feed/other), F10 never forces it"
            }
        )
    });
}

/// The probed description of a G-buffer. No GL query: the bind thunk captured it
/// when the target was first bound.
fn cached_gbuffer(fbo: u32) -> Option<Gbuffer> {
    GBUFFERS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&fbo)
        .copied()
}

/// The G-buffer paired with a tonemap target (learned at the quad), with its
/// attachment info, if it is still known and usable.
fn paired_gbuffer(target: u32) -> Option<Gbuffer> {
    let gbuffer = {
        let mut targets = TARGETS.lock().unwrap_or_else(|p| p.into_inner());
        targets.prune(now_ms());
        targets.target_gbuf.get(&target).copied()
    }?;
    let entry = cached_gbuffer(gbuffer)?;
    if entry.tex_normal == 0 || entry.tex_color == 0 {
        return None;
    }
    Some(entry)
}

/// The tonemap targets seen at a quad, for skip diagnostics.
fn known_targets() -> Vec<u32> {
    let mut targets = TARGETS.lock().unwrap_or_else(|p| p.into_inner());
    targets.prune(now_ms());
    let mut known: Vec<u32> = targets.by_fbo.keys().copied().collect();
    known.sort_unstable();
    known
}

/// The tonemap quad draw (`0x68BB00`, recognised by its return address): the end
/// of a view's GL sequence. The last G-buffer bound before it and the framebuffer
/// it draws into are that view's pair, the target inherits this quad's own IR
/// state, and the quad is replaced with the composite when the target is IR-graded
/// (or F10 forces a target learned as the main view).
unsafe fn tonemap_quad(
    ir: bool,
    camera: usize,
    quad: (u32, i32, u32, *const c_void),
) -> bool {
    let target = CURRENT_FBO.load(Ordering::Relaxed);
    let gbuffer = LAST_GBUF.swap(0, Ordering::AcqRel);
    // The next framebuffer bind identifies this view (FBO 0 = main, see
    // `note_first_bind`); a quad into target 0 already is the main view.
    if target != 0 {
        PENDING_QUAD_TARGET.store(target, Ordering::Release);
    }
    let now = now_ms();
    let main = note_quad(gbuffer, target, ir, camera, now);
    diag_tick(now);
    if main {
        dump_step();
    }
    // The next view uploads its own matrices; a view that never does must not
    // replay this one's transform.
    VIEW_PROJ_VALID.store(false, Ordering::Relaxed);
    WORLD_VALID.store(false, Ordering::Relaxed);
    if ir {
        return composite_view(target, ir, false, main, camera, quad);
    }
    // F10 (debug) forces only a target learned as the main view; a camera feed
    // renders FLIR only when its own tonemap params say IR.
    let forced = cfg!(debug_assertions) && main && f10_toggled();
    if !forced {
        return false;
    }
    composite_view(target, ir, forced, main, camera, quad)
}

/// Log once per distinct `(view size, target, IR state, F10 state, view kind)`.
/// Called at the tonemap quad draw, where the viewport is this view's own size.
#[cfg(feature = "logging")]
fn log_view_mode(viewport: [i32; 4], target: u32, ir: bool, forced: bool, main: bool) {
    let (width, height) = (viewport[2], viewport[3]);
    let key = (1u64 << 60)
        | (((width as u32 as u64) & 0xffff) << 44)
        | (((height as u32 as u64) & 0xffff) << 28)
        | (((target as u64) & 0xffff) << 12)
        | ((ir as u64) << 1)
        | forced as u64;
    log_once_key(key, || {
        format!(
            "FLIR: tonemap quad view {}x{} at ({}, {}) target {target} IR={} F10={} ({} view)",
            width,
            height,
            viewport[0],
            viewport[1],
            ir as u8,
            forced as u8,
            if main { "main" } else { "feed/other" }
        )
    });
}

// ---------------------------------------------------------------------------
// glDrawElements thunk
// ---------------------------------------------------------------------------

// The game calls `glDrawElements` through the IAT slot `0x140A935F8`, which is a
// plain pointer variable. The thunk needs the caller's return address to
// recognize the tonemap quad draw, which is not an argument: this stub reads it
// off the stack and passes it as a fifth argument to the Rust body.
//
// Windows x64: at entry `[rsp]` is the return address; the fifth argument goes
// in the caller-allocated shadow-space slot `[rsp+0x20]` (which is
// `[callee_rsp+0x28]`). The 0x28 allocation keeps the stack 16-byte aligned
// before the call.
core::arch::global_asm!(
    ".globl swi_gl_draw_elements_entry",
    "swi_gl_draw_elements_entry:",
    "sub rsp, 0x28",
    "mov rax, [rsp+0x28]",
    "mov [rsp+0x20], rax",
    "call {body}",
    "add rsp, 0x28",
    "ret",
    body = sym gl_draw_elements_entry_rust,
);

extern "system" {
    fn swi_gl_draw_elements_entry();
}

/// Adds the draw thunk's own time (including the game's forwarded draw call,
/// which is cheap on a deferred driver) to `HOOK_US` on drop.
#[cfg(feature = "logging")]
struct HookTimer(u64);
#[cfg(feature = "logging")]
impl Drop for HookTimer {
    fn drop(&mut self) {
        HOOK_US.fetch_add(now_us().saturating_sub(self.0), Ordering::Relaxed);
    }
}

#[no_mangle]
pub(crate) extern "system" fn gl_draw_elements_entry_rust(
    mode: u32,
    count: i32,
    kind: u32,
    indices: *const c_void,
    return_address: usize,
) {
    let _stage = fault::enter(stage::FLIR_GL_DRAW_ELEMENTS);
    // Measures the plugin's own work only: dropped before the game's draw is
    // forwarded (it used to include the driver's draw cost).
    #[cfg(feature = "logging")]
    let timer = HookTimer(now_us());
    let original = ORIG_GL_DRAW_ELEMENTS.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    let f: unsafe extern "system" fn(u32, i32, u32, *const c_void) =
        unsafe { std::mem::transmute(original) };

    let base = GL_BASE.load(Ordering::Relaxed);
    let in_tonemap_quad = base != 0
        && return_address >= base + RVA_TONEMAP_QUAD
        && return_address < base + RVA_TONEMAP_QUAD_END;
    if in_tonemap_quad && !IN_SUBSTITUTE.load(Ordering::Acquire) {
        // `0x68BB00` is a shared fullscreen-quad helper, so several passes draw
        // through it: the tonemap uniforms pass (0x68B8C0) marks the quad that
        // carries the graded image and carries this view's IR decision, and only
        // that one learns a pairing or is replaced. The pair/G-buffer/viewport are
        // all read here, in GL order, so submit order cannot point them at another
        // view. Without the uniforms hook every matched quad is treated as a
        // tonemap quad, but no view is ever IR-graded.
        let pending = TONEMAP_NEXT.swap(0, Ordering::AcqRel);
        let is_tonemap = pending != 0 || !TONEMAP_HOOK_OK.load(Ordering::Relaxed);
        if is_tonemap {
            // A failure or panic leaves the game's own draw to run, after the
            // state guard has put everything back.
            let camera = TONEMAP_CAMERA.load(Ordering::Relaxed);
            let replaced =
                std::panic::catch_unwind(|| unsafe { tonemap_quad(pending == 2, camera, (mode, count, kind, indices)) })
                    .unwrap_or(false);
            if replaced {
                return;
            }
        }
    }

    unsafe { snapshot_light_textures() };
    let substituted = !IN_SUBSTITUTE.load(Ordering::Acquire)
        && ir_view_active()
        && std::panic::catch_unwind(|| unsafe { flir_block_draw_traced(mode, count, kind, indices) })
            .unwrap_or_else(|_| {
                // A half-drawn substitute leaves untrusted GL state behind: keep
                // the game's own draw from now on (fail closed).
                BLOCK_FAILED.store(true, Ordering::Relaxed);
                false
            });
    if substituted {
        #[cfg(feature = "logging")]
        HOOK_SUB_US.fetch_add(now_us().saturating_sub(timer.0), Ordering::Relaxed);
        return;
    }
    #[cfg(feature = "logging")]
    drop(timer);
    unsafe { f(mode, count, kind, indices) };
    if RESTORE_LF.load(Ordering::Relaxed) >= 0 {
        unsafe { restore_skinned_draw() };
    }
}

/// GL `glBindFramebuffer` thunk: keeps the GL-order view state. `CURRENT_FBO` is
/// the draw target a draw is attributed to, `LAST_GBUF` the newest G-buffer-like
/// target bound since the last tonemap quad (the pairing is learned at that
/// quad). Forwards unchanged.
pub(crate) extern "C" fn gl_bind_framebuffer_hook(target: u32, framebuffer: u32) {
    let _stage = fault::enter(stage::FLIR_GL_BIND_FRAMEBUFFER);
    let original = ORIG_GL_BIND_FRAMEBUFFER.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    let f: unsafe extern "system" fn(u32, u32) = unsafe { std::mem::transmute(original) };
    unsafe { f(target, framebuffer) };
    if target != GL_FRAMEBUFFER && target != GL_DRAW_FRAMEBUFFER {
        return;
    }
    CURRENT_FBO.store(framebuffer, Ordering::Relaxed);
    let _ = std::panic::catch_unwind(|| update_active_view(framebuffer));
    // The first bind after a tonemap quad identifies that view: FBO 0 is the main
    // view (FXAA presents into the default framebuffer), anything else is a feed
    // or other offscreen view. Consuming it here keeps the association in GL
    // command order.
    let pending = PENDING_QUAD_TARGET.swap(0, Ordering::AcqRel);
    if pending != 0 {
        let _ = std::panic::catch_unwind(|| note_first_bind(pending, framebuffer));
    }
    if framebuffer == 0 {
        return;
    }
    // A panic must never unwind through the GL thunk. Known targets come from
    // the registry (no GL queries); an unknown one is probed once, while it is
    // bound, and only kept when it has the two colour attachments a G-buffer has.
    let _ = std::panic::catch_unwind(|| {
        if let Some(gl) = gl() {
            if unsafe { gbuffer_for(gl, framebuffer) }.is_some() {
                LAST_GBUF.store(framebuffer, Ordering::Relaxed);
            }
        }
    });
}

/// GL `glUniformMatrix4fv` thunk: captures the last `mat_view_proj`/`mat_world`
/// upload for the current program (uniform names are queryable on any program
/// object), so the block pass needs no guessed location.
pub(crate) extern "C" fn gl_uniform_matrix4fv_hook(
    location: i32,
    count: i32,
    transpose: u8,
    value: *const f32,
) {
    let _stage = fault::enter(stage::FLIR_GL_UNIFORM_MATRIX);
    let original = ORIG_GL_UNIFORM_MATRIX4FV.load(Ordering::SeqCst);
    if original != 0 {
        if vp_recording() && count == 1 && location >= 0 && !value.is_null() {
            // A panic must never unwind through the GL thunk.
            // No VirtualQuery on `value`: the driver reads the same 64 bytes in
            // the forwarded call right below, so a bad pointer faults either way.
            let _ = std::panic::catch_unwind(|| record_matrix_upload(location, transpose, value));
        }
        let f: unsafe extern "system" fn(i32, i32, u8, *const f32) =
            unsafe { std::mem::transmute(original) };
        unsafe { f(location, count, transpose, value) };
    }
}

/// Only record the matrices while the FLIR block pass could actually replay them:
/// an IR-graded target exists (or a debug F10 view is on) and the mesh->body map
/// is not empty. A matrix upload costs a per-draw `GL_CURRENT_PROGRAM` query, so
/// this stays off while no substitution can ever happen.
fn vp_recording() -> bool {
    !MESH_BODY_EMPTY.load(Ordering::Relaxed)
        && (IR_WANTED.load(Ordering::Relaxed)
            || (cfg!(debug_assertions) && F10_ON.load(Ordering::Relaxed)))
}

/// Capture `mat_view_proj` and `mat_world` for the block pass. The raw 16 floats
/// and the game's transpose flag are stored, so the block program replays an
/// identical transform.
/// Deferred light program id, its graphics_offset location, the last
/// inverse view-projection it was given (column-major) and graphics_offset.y.
static LIGHT_PROGRAM: AtomicU32 = AtomicU32::new(0);
static LIGHT_OFFSET_LOC: AtomicI32 = AtomicI32::new(-1);
static LIGHT_INV: Mutex<Option<[f32; 16]>> = Mutex::new(None);
/// texture_water_depth: the ocean surface's log depth (depth_ocean pass), the
/// game's own "is this pixel under water" input. Snapshotted at the light draw.
static LIGHT_WATER_LOC: AtomicI32 = AtomicI32::new(-1);
static LIGHT_WATER_UNIT: AtomicI32 = AtomicI32::new(-1);
static LIGHT_WATER_TEX: AtomicU32 = AtomicU32::new(0);

/// At the light pass's own draw: which texture is its water depth.
unsafe fn snapshot_light_textures() {
    let light = LIGHT_PROGRAM.load(Ordering::Relaxed);
    if light == 0 || TRACK_PROGRAM.load(Ordering::Relaxed) as u32 != light {
        return;
    }
    let mut unit = LIGHT_WATER_UNIT.load(Ordering::Relaxed);
    if unit < 0 {
        let Some(gl) = gl() else { return };
        let loc = LIGHT_WATER_LOC.load(Ordering::Relaxed);
        if loc < 0 {
            return;
        }
        // Once per light program: the sampler's unit (5 in the RE notes).
        (gl.get_uniformiv)(light, loc, &mut unit);
        LIGHT_WATER_UNIT.store(unit, Ordering::Relaxed);
        log_debug(|| format!("FLIR: light pass texture_water_depth on unit {unit}"));
    }
    if (0..16).contains(&unit) {
        LIGHT_WATER_TEX.store(TEX_UNITS[unit as usize].load(Ordering::Relaxed), Ordering::Relaxed);
    }
}
static LIGHT_OFFSET_Y_BITS: AtomicU32 = AtomicU32::new(0);
/// The light pass's own sun_light_direction (points from the sun into the
/// scene): what the game lights and shadows with this frame.
static LIGHT_SUN_LOC: AtomicI32 = AtomicI32::new(-1);
static LIGHT_SUN: Mutex<Option<[f32; 3]>> = Mutex::new(None);
static ORIG_GL_UNIFORM3F: AtomicUsize = AtomicUsize::new(0);

/// `glUniform3f` thunk: records the light pass's graphics_offset (render
/// origin in world space; the sea is world y = 0).
pub(crate) extern "C" fn gl_uniform3f_hook(location: i32, x: f32, y: f32, z: f32) {
    let original = ORIG_GL_UNIFORM3F.load(Ordering::Relaxed);
    if original != 0 {
        let f: unsafe extern "system" fn(i32, f32, f32, f32) = unsafe { std::mem::transmute(original) };
        unsafe { f(location, x, y, z) };
    }
    if location < 0 || TRACK_PROGRAM.load(Ordering::Relaxed) as u32 != LIGHT_PROGRAM.load(Ordering::Relaxed) {
        return;
    }
    if location == LIGHT_OFFSET_LOC.load(Ordering::Relaxed) {
        LIGHT_OFFSET_Y_BITS.store(y.to_bits(), Ordering::Relaxed);
    } else if location == LIGHT_SUN_LOC.load(Ordering::Relaxed) {
        let n = (x * x + y * y + z * z).sqrt();
        if n.is_finite() && n > 1e-6 {
            *LIGHT_SUN.lock().unwrap_or_else(|p| p.into_inner()) = Some([x / n, y / n, z / n]);
        }
    }
}

fn record_matrix_upload(location: i32, transpose: u8, value: *const f32) {
    let Some(gl) = gl() else { return };
    let program = unsafe { tracked(gl, &TRACK_PROGRAM, GL_CURRENT_PROGRAM) };
    if program <= 0 {
        return;
    }
    let program = program as u32;
    let mut mats = PROG_MATS.lock().unwrap_or_else(|p| p.into_inner());
    if !mats.contains_key(&program) {
        // First upload seen for this program: two location queries, once.
        drop(mats);
        let locs = unsafe {
            (
                glq!(gl.get_uniform_location, program, b"mat_view_proj\0".as_ptr() as *const i8),
                glq!(gl.get_uniform_location, program, b"mat_world\0".as_ptr() as *const i8),
            )
        };
        // The deferred light pass (light.glslf) is the only program with both
        // texture_water_depth and graphics_offset: its inverse view-projection
        // and render origin let the composite cast rays (sea plane, sun disc).
        let light_inv = unsafe {
            let water = glq!(gl.get_uniform_location, program, b"texture_water_depth\0".as_ptr() as *const i8);
            let offset = glq!(gl.get_uniform_location, program, b"graphics_offset\0".as_ptr() as *const i8);
            if water >= 0 && offset >= 0 {
                LIGHT_PROGRAM.store(program, Ordering::Relaxed);
                LIGHT_OFFSET_LOC.store(offset, Ordering::Relaxed);
                LIGHT_WATER_LOC.store(water, Ordering::Relaxed);
                LIGHT_WATER_UNIT.store(-1, Ordering::Relaxed);
                LIGHT_SUN_LOC.store(
                    glq!(gl.get_uniform_location, program, b"sun_light_direction\0".as_ptr() as *const i8),
                    Ordering::Relaxed,
                );
                glq!(gl.get_uniform_location, program, b"mat_view_proj_inverse\0".as_ptr() as *const i8)
            } else {
                -1
            }
        };
        mats = PROG_MATS.lock().unwrap_or_else(|p| p.into_inner());
        if mats.len() >= 1024 {
            mats.clear();
        }
        mats.insert(program, ProgMats { locs, light_inv, ..ProgMats::default() });
    }
    let Some(e) = mats.get_mut(&program) else { return };
    let (view_proj_loc, world_loc) = e.locs;
    if location >= 0 && location == e.light_inv {
        drop(mats);
        let raw: [f32; 16] = unsafe { std::ptr::read_unaligned(value as *const [f32; 16]) };
        // Same view as the tonemap quad that follows (light pass runs per view
        // right before it).
        *LIGHT_INV.lock().unwrap_or_else(|p| p.into_inner()) = Some(to_colmajor(&raw, transpose));
        return;
    }
    if location != view_proj_loc && location != world_loc {
        return;
    }
    let raw: [f32; 16] = unsafe { std::ptr::read_unaligned(value as *const [f32; 16]) };
    if location == view_proj_loc {
        e.vp = Some((raw, transpose));
    } else {
        e.world = Some((raw, transpose));
    }
    drop(mats);
    log_once(LOG_FLIR_VP, || {
        "FLIR: mat_view_proj/mat_world captured from glUniformMatrix4fv by name".to_string()
    });
}

// ---------------------------------------------------------------------------
// View selection
// ---------------------------------------------------------------------------

/// True when the draw that is running belongs to a view we replace. A G-buffer is
/// identified by the pairing learned at its view's previous tonemap quad; a
/// G-buffer that is not paired yet (or whose target is not IR-forced) is never
/// substituted, because its target - and therefore its view - is unknown.
fn ir_view_active() -> bool {
    // An empty mesh->body map means no draw can be substituted, so the whole
    // block pass (per-draw GL queries, map lock) stays out of the game's draws.
    if MESH_BODY_EMPTY.load(Ordering::Relaxed)
        || BLOCKPASS_DISABLED.load(Ordering::Relaxed)
        || BLOCK_FAILED.load(Ordering::Relaxed)
        || COMPOSITE_FAILED.load(Ordering::Relaxed)
    {
        return false;
    }
    CURRENT_FBO.load(Ordering::Relaxed) != 0 && ACTIVE_VIEW.load(Ordering::Relaxed) != 0
}

/// F10 edge-detect (debug builds only, per the design contract).
fn f10_toggled() -> bool {
    if !cfg!(debug_assertions) {
        return false;
    }
    static GET_KEY_STATE: OnceLock<usize> = OnceLock::new();
    let address = *GET_KEY_STATE.get_or_init(|| unsafe {
        let module = LoadLibraryA(b"user32.dll\0".as_ptr());
        if module.is_null() {
            return 0;
        }
        GetProcAddress(module, b"GetAsyncKeyState\0".as_ptr()).map_or(0, |f| f as usize)
    });
    if address == 0 {
        return F10_ON.load(Ordering::Relaxed);
    }
    // Poll at most every 8 ms: it is read from the per-draw gate and per quad.
    static LAST_POLL_US: AtomicU64 = AtomicU64::new(0);
    let now = now_us();
    if now.saturating_sub(LAST_POLL_US.load(Ordering::Relaxed)) < 8000 {
        return F10_ON.load(Ordering::Relaxed);
    }
    LAST_POLL_US.store(now, Ordering::Relaxed);
    let f: unsafe extern "system" fn(i32) -> i16 = unsafe { std::mem::transmute(address) };
    let down = (unsafe { f(0x79) } as u16) & 0x8000 != 0; // VK_F10
    if down && !KEY_HELD.swap(true, Ordering::Relaxed) {
        let now_on = !F10_ON.load(Ordering::Relaxed);
        F10_ON.store(now_on, Ordering::Relaxed);
        // The forced-G-buffer mirror follows the toggle immediately, so the next
        // draws of the main view already use it.
        refresh_forced_gbuffers();
        // Once per distinct state: an F10 that never reaches the view has to be
        // distinguishable from a composite that is drawn but invisible.
        log_once_key((4u64 << 56) | now_on as u64, || {
            format!(
                "FLIR: F10 edge detected -> debug view {} (ir_view_active={})",
                if now_on { "on" } else { "off" },
                ir_view_active()
            )
        });
    } else if !down {
        KEY_HELD.store(false, Ordering::Relaxed);
    }
    F10_ON.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Mesh -> body map
// ---------------------------------------------------------------------------

/// Element budget of one mesh-map walk. The walk runs every 250 ms on the GAME
/// thread (see `scene_tick`) and each element costs a `readable` (VirtualQuery)
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
unsafe fn element_mesh(element: usize) -> Option<(usize, u32)> {
    if element < PTR_MIN || !readable(element, 8) {
        return None;
    }
    let group = read_usize_at(element);
    if group < PTR_MIN || !readable(group + 0x08, 8) {
        return None;
    }
    let mesh = read_usize_at(group + 0x08);
    if mesh < PTR_MIN || !readable(mesh + MESH_IBO, 4) {
        return None;
    }
    let ibo = read_u32_at(mesh + MESH_IBO);
    (ibo != 0).then_some((mesh, ibo))
}

/// Element addresses of a game ringvec `{ptr, u32 cap, u32 start, u32 count}`.
/// Empty when the header is implausible or the storage is unreadable.
unsafe fn ringvec_elems(base: usize, stride: usize) -> Vec<usize> {
    if base < PTR_MIN || !readable(base, 0x14) {
        return Vec::new();
    }
    let ptr = read_usize_at(base);
    let cap = read_u32_at(base + 8);
    let start = read_u32_at(base + 0xC);
    let count = read_u32_at(base + 0x10);
    if ptr < PTR_MIN || cap == 0 || cap > MAX_CHUNK_ITEMS || count > cap || !readable(ptr, cap as usize * stride) {
        return Vec::new();
    }
    (0..count)
        .map(|i| ptr + stride * ((i as usize + start as usize) % cap as usize))
        .collect()
}

fn rebuild_mesh_body(scene: usize) {
    unsafe {
        let mut map: HashMap<u32, usize> = HashMap::default();
        let mut owner: HashMap<u32, (usize, usize)> = HashMap::default();
        let (mut coll_mesh, mut coll_body) = (0u32, 0u32);
        let (mut bodies, mut meshes) = (0usize, 0usize);
        let mut walked = 0u32;
        crate::blocktemp::for_each_body(scene, |_vehicle_id, body| {
            if walked >= MAX_MESH_ELEMENTS {
                return false;
            }
            // re_chunks: body+0x540 and body+0x588 are ringvecs of 88-byte chunk
            // entries; each entry starts with an inline ringvec of 96-byte
            // instances holding three geometry pointers (+0/+0x10/+0x20); the GL
            // holder is *(res+8) with the IBO at +0x64.
            bodies += 1;
            let mut body_ibos = 0u32;
            let mut elements: Vec<usize> = Vec::new();
            for table in [BODY_CHUNK_ITEMS, BODY_CHUNK_ITEMS_B] {
                for entry in ringvec_elems(body + table, 88) {
                    for inst in ringvec_elems(entry, 96) {
                        for slot in [0usize, 0x10, 0x20] {
                            if readable(inst + slot, 8) {
                                elements.push(inst + slot);
                            }
                        }
                    }
                }
            }
            for element in elements {
                if walked >= MAX_MESH_ELEMENTS {
                    return false;
                }
                walked += 1;
                let Some((mesh, ibo)) = element_mesh(element) else {
                    continue;
                };
                if let Some(&(m, b)) = owner.get(&ibo) {
                    if m != mesh {
                        coll_mesh += 1;
                    }
                    if b != body {
                        coll_body += 1;
                    }
                    if (m != mesh || b != body) && coll_mesh + coll_body <= 8 {
                        log_debug(|| {
                            format!(
                                "FLIR: IBO collision ibo={ibo}: mesh {m:#x}/body {b:#x} vs \
mesh {mesh:#x}/body {body:#x} (later wins)"
                            )
                        });
                    }
                } else {
                    body_ibos += 1;
                }
                owner.insert(ibo, (mesh, body));
                map.insert(ibo, body);
                meshes += 1;
            }
            if body_ibos > 0 {
                log_once_key(body as u64, || {
                    format!("FLIR: body {body:#x} mapped {body_ibos} mesh IBOs")
                });
            }
            true
        });
        let count = map.len();
        if coll_mesh + coll_body > 0 {
            log_debug(|| {
                format!(
                    "FLIR: IBO collisions: same-ibo-different-mesh={coll_mesh} \
same-ibo-different-body={coll_body} (meshes walked {meshes}, unique ibos {count})"
                )
            });
        }
        *MESH_BODY.lock().unwrap_or_else(|p| p.into_inner()) = map;
        MESH_BODY_EMPTY.store(count == 0, Ordering::Relaxed);
        if count > 0 {
            log_once(LOG_FLIR_MESH_MAP, || {
                format!(
                    "FLIR: mesh->body map from scene {scene:#x} (game thread, keyed by IBO id): \
bodies={bodies} meshes={count} (elements walked {walked})"
                )
            });
        } else {
            log_once(LOG_FLIR_MESH_MAP, || {
                format!(
                    "FLIR: mesh->body map EMPTY (bodies={bodies}); \
body+{BODY_CHUNK_ITEMS:#x} / container layout differs, block pass disabled"
                )
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Block pass
// ---------------------------------------------------------------------------

/// Re-issue a vehicle chunk draw with the FLIR block program. Returns false when
/// the draw must be left to the game (nothing is changed in that case).
unsafe fn flir_block_draw(
    mode: u32,
    count: i32,
    kind: u32,
    indices: *const c_void,
    t: &mut Trace,
) -> bool {
    t.reason = SK_BAD_DRAW;
    // Only the mesh's own indexed triangle draw qualifies. `indices` is NOT
    // checked: it is a byte offset into the bound IBO (`first * 2`), so a valid
    // first == 0 draw passes NULL, and we never dereference it.
    let block_shape = mode == GL_TRIANGLES && kind == GL_UNSIGNED_SHORT && count > 0;
    t.reason = SK_VIEW;
    let Some(gl) = gl() else { return false };
    // G-buffer pass only: never draw our normal/temperature program into a
    // depth/shadow target, and never into a G-buffer whose target is not
    // IR-forced. The bound draw framebuffer is the one tracked in GL order (no
    // driver query), and the pairing was learned from a previous frame's tonemap
    // quad - an unpaired G-buffer is never substituted in.
    let fbo = CURRENT_FBO.load(Ordering::Relaxed);
    if fbo == 0 || !ir_view_active() {
        return false;
    }
    {
        // Classify each program once (cached), so skinned draws are known even
        // before any vehicle chunk has been drawn in this view.
        static LAST_CLASSIFIED: AtomicU32 = AtomicU32::new(0);
        let program = tracked(gl, &TRACK_PROGRAM, GL_CURRENT_PROGRAM);
        if program > 0 && LAST_CLASSIFIED.swap(program as u32, Ordering::Relaxed) != program as u32 {
            let _ = opaque_program(gl, program as u32);
        }
        if program > 0 && count > 0 {
            if let Some((slot, p)) = particle_slot(program as u32) {
                return match cached_gbuffer(fbo) {
                    Some(g) if !g.normal_encoded => particle_draw(gl, slot, p, mode, count, kind, indices),
                    _ => false,
                };
            }
        }
        if program > 0 && SKINNED[0].0.load(Ordering::Relaxed) != 0 && skinned_slot(program as u32).is_some() {
            if cached_gbuffer(fbo).is_some() {
                mark_skinned_draw(gl, program as u32);
            }
            return false;
        }
    }
    t.reason = SK_BAD_DRAW;
    // Block substitution: only the mesh's own indexed triangle draw qualifies.
    if !block_shape {
        return false;
    }
    // The game binds the mesh's IBO before the draw; that id is the map key. It
    // is checked before any framebuffer lookup, so a pass full of non-vehicle
    // draws costs only one GL query each.
    t.reason = SK_NO_IBO;
    let ibo = tracked(gl, &TRACK_IBO, GL_ELEMENT_ARRAY_BUFFER_BINDING);
    t.ibo = ibo;
    if ibo <= 0 {
        return false;
    }
    let mapped = {
        let map = MESH_BODY.lock().unwrap_or_else(|p| p.into_inner());
        match map.get(&(ibo as u32)) {
            Some(body) => Some(*body),
            None => {
                let entries = map.len();
                drop(map);
                diag_count(DIAG_UNMAPPED);
                // Not a chunk IBO: possibly an individually-meshed component
                // (resolved by position below); otherwise the game draw is kept.
                log_once(LOG_FLIR_MESH_MISS, || {
                    format!(
                        "FLIR: draw IBO {ibo} is not in the mesh->body map ({entries} entries); \
tried as a component draw, else the game draw is kept (body+{BODY_CHUNK_ITEMS:#x} layout may differ)"
                    )
                });
                // Cheap rejection: no chunk drawn yet -> no body to match.
                if !has_body_mats() {
                    t.reason = SK_NO_MATS;
                    return false;
                }
                None
            }
        }
    };
    t.mapped = mapped.is_some();
    t.reason = SK_NO_GBUF;
    let Some(gbuffer) = cached_gbuffer(fbo) else {
        return false;
    };
    let program = tracked(gl, &TRACK_PROGRAM, GL_CURRENT_PROGRAM);
    t.program = program;
    t.reason = SK_PROGRAM;
    if program <= 0 || !opaque_program(gl, program as u32) {
        return false;
    }
    // Read both matrices from the game's program itself: a uniform persists per
    // program, so the last `glUniformMatrix4fv` seen by the hook (any program)
    // is not necessarily the value this draw uses when the game skips redundant
    // uploads and interleaves programs (draw-order, hence angle, dependent).
    let cached = PROG_MATS.lock().unwrap_or_else(|p| p.into_inner()).get(&(program as u32)).copied();
    match cached {
        Some(ProgMats { vp: Some((vp, vpt)), world: Some((w, wt)), .. }) => {
            *VIEW_PROJ.lock().unwrap_or_else(|p| p.into_inner()) = vp;
            VIEW_PROJ_TRANSPOSE.store(vpt as u32, Ordering::Relaxed);
            VIEW_PROJ_VALID.store(true, Ordering::Relaxed);
            *WORLD_MATRIX.lock().unwrap_or_else(|p| p.into_inner()) = w;
            WORLD_TRANSPOSE.store(wt as u32, Ordering::Relaxed);
            WORLD_VALID.store(true, Ordering::Relaxed);
        }
        // Never replay another program's matrices.
        _ => {
            t.reason = SK_NO_MATRIX;
            return false;
        }
    }
    t.reason = SK_NO_MATRIX;
    if !VIEW_PROJ_VALID.load(Ordering::Relaxed) || !WORLD_VALID.load(Ordering::Relaxed) {
        log_once(LOG_FLIR_VP_MISS, || {
            "FLIR: mat_view_proj/mat_world not captured yet; block pass skipped".to_string()
        });
        return false;
    }
    let world_raw = *WORLD_MATRIX.lock().unwrap_or_else(|p| p.into_inner());
    let world_cm = to_colmajor(&world_raw, WORLD_TRANSPOSE.load(Ordering::Relaxed) as u8);
    t.reason = SK_NO_COMPONENT;
    let (body, comp_to_body) = match mapped {
        Some(body) => {
            record_body_mat(body, world_cm);
            (body, IDENTITY)
        }
        None => match resolve_component(gl, program as u32, ibo as u32, count, indices, &world_cm) {
            Some(found) => found,
            None => return false,
        },
    };
    t.body = body;
    let component = mapped.is_none();
    t.reason = SK_BEGIN;
    // Breadcrumb for anything in the substitute (texture upload, our GL calls,
    // the game's driver) that can still fault.
    fault::set(stage::FLIR_BLOCK_DRAW);
    IN_SUBSTITUTE.store(true, Ordering::Release);
    // A panic (a bogus grid, a GL call that traps) unwinds through the caller's
    // catch_unwind; the guard then puts the game's program/texture back so the
    // game's own draw is not forwarded with our program still bound.
    let mut guard = StateGuard {
        gl,
        state: begin_block_draw(
            gl,
            body,
            ibo as u32,
            count,
            indices,
            gbuffer.normal_encoded,
            &comp_to_body,
        ),
    };
    if guard.state.is_none() {
        return false;
    }
    (gl.draw_elements)(mode, count, kind, indices);
    if let Some(state) = guard.state.take() {
        end_block_draw(gl, state);
    }
    diag_count(if component { DIAG_COMPONENT } else { DIAG_SUBSTITUTED });
    t.reason = SK_OK;
    true
}

// ---------------------------------------------------------------------------
// Per-reason skip counters (logging builds) and the F8 one-frame draw dump
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
const SK_NAMES: [&str; SK_N] = [
    "substituted", "not_tri_u16", "no_ibo", "unmapped_no_bodymats", "no_gbuffer",
    "not_opaque_prog", "no_matrix", "component_unresolved", "begin_failed", "view_inactive",
];

/// What one draw hit in [`flir_block_draw`].
struct Trace {
    reason: u8,
    ibo: i32,
    program: i32,
    mapped: bool,
    count: i32,
    body: usize,
}

#[cfg(feature = "logging")]
static SKIP_COUNTS: [[AtomicU32; SK_N]; 2] = [
    [const { AtomicU32::new(0) }; SK_N],
    [const { AtomicU32::new(0) }; SK_N],
];

/// 0 = idle, 1 = armed (waits for a main-view quad), 2 = recording the next
/// main-view frame.
static DUMP_STATE: AtomicU32 = AtomicU32::new(0);
static F8_HELD: AtomicBool = AtomicBool::new(false);
/// (ibo, reason, fbo, program, mapped, view, body) -> (draws, index total, max index count)
type DumpKey = (i32, u8, u32, i32, bool, u32, usize);
static DUMP: Mutex<Vec<(DumpKey, (u32, u64, i32))>> = Mutex::new(Vec::new());

unsafe fn flir_block_draw_traced(
    mode: u32,
    count: i32,
    kind: u32,
    indices: *const c_void,
) -> bool {
    let mut t = Trace { reason: SK_VIEW, ibo: 0, program: 0, mapped: false, count, body: 0 };
    let done = flir_block_draw(mode, count, kind, indices, &mut t);
    if ACTIVE_VIEW.load(Ordering::Relaxed) == 1 {
        #[cfg(feature = "logging")]
        SKIP_COUNTS[t.mapped as usize][t.reason as usize].fetch_add(1, Ordering::Relaxed);
        if DUMP_STATE.load(Ordering::Relaxed) == 2 {
            let key = (
                t.ibo,
                t.reason,
                CURRENT_FBO.load(Ordering::Relaxed),
                t.program,
                t.mapped,
                ACTIVE_VIEW.load(Ordering::Relaxed),
                t.body,
            );
            let mut dump = DUMP.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((_, v)) = dump.iter_mut().find(|(k, _)| *k == key) {
                v.0 += 1;
                v.1 += t.count.max(0) as u64;
                v.2 = v.2.max(t.count);
            } else if dump.len() < 4096 {
                dump.push((key, (1, t.count.max(0) as u64, t.count)));
            }
        }
    }
    done
}

/// Called at every main-view tonemap quad (a frame boundary of the main view):
/// F8 edge arms the dump, the next boundary starts recording, the one after
/// prints it.
fn dump_step() {
    if !cfg!(debug_assertions) {
        return;
    }
    if let Some(down) = async_key_down(0x77) {
        if down && !F8_HELD.swap(true, Ordering::Relaxed) {
            if DUMP_STATE.load(Ordering::Relaxed) == 0 {
                DUMP_STATE.store(1, Ordering::Relaxed);
                log_line("FLIR: F8 draw dump armed (needs F10 on)");
            }
        } else if !down {
            F8_HELD.store(false, Ordering::Relaxed);
        }
    }
    match DUMP_STATE.load(Ordering::Relaxed) {
        1 => {
            DUMP.lock().unwrap_or_else(|p| p.into_inner()).clear();
            DUMP_STATE.store(2, Ordering::Relaxed);
        }
        2 => {
            DUMP_STATE.store(0, Ordering::Relaxed);
            let mut rows = std::mem::take(&mut *DUMP.lock().unwrap_or_else(|p| p.into_inner()));
            rows.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
            let total: u32 = rows.iter().map(|r| r.1 .0).sum();
            let (map_n, mats) = (
                MESH_BODY.lock().unwrap_or_else(|p| p.into_inner()).len(),
                BODY_MATS.lock().unwrap_or_else(|p| p.into_inner()).len(),
            );
            let mut out = format!(
                "FLIR: F8 main-view draw dump: {total} draws, {} groups, map={map_n} \
body_mats={mats} frame={} (top 58 by count; ibo reason fbo program map/comp view body draws idx max_idx)",
                rows.len(),
                FRAME.load(Ordering::Relaxed)
            );
            for ((ibo, reason, fbo, program, mapped, view, body), (n, idx, max)) in rows.iter().take(58) {
                out.push_str(&format!(
                    "\n  ibo={ibo} {} fbo={fbo} prog={program} {} view={view} body={body:#x} n={n} idx={idx} max={max}",
                    SK_NAMES[*reason as usize],
                    if *mapped { "map" } else { "comp" },
                ));
            }
            log_line(&out);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Component draws (individually-meshed parts): body identified by position
// ---------------------------------------------------------------------------

const IDENTITY: [f32; 16] = [
    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
];
const MAX_BODY_MATS: usize = 64;

/// A substituted body's chunk `mat_world` (column-major, camera-origin-relative).
struct BodyMat {
    frame: u64,
    view: u32,
    body: usize,
    mat: [f32; 16],
    /// Squared distance (m^2) from the body origin past which nothing can be
    /// in its grid; INFINITY until the body has a grid. Refreshed each frame.
    reach2: f32,
}

/// Squared reach of a grid from its body origin (farthest corner + 1 voxel).
fn grid_reach2(min: [i32; 3], dims: [u32; 3]) -> f32 {
    (0..3)
        .map(|i| {
            let m = (min[i] - 1).abs().max((min[i] + dims[i] as i32 + 1).abs()) as f32 * 0.25;
            m * m
        })
        .sum()
}

fn body_reach2(body: usize) -> f32 {
    GRID_TEXTURES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&body)
        .map_or(f32::INFINITY, |g| grid_reach2(g.min, g.dims))
}

/// Newest chunk matrix per (view, body); entries older than the previous frame
/// are dropped, so a lookup sees this frame's matrices (fallback: last frame's).
static BODY_MATS: Mutex<Vec<BodyMat>> = Mutex::new(Vec::new());
#[cfg(feature = "logging")]
static COMP_LOGGED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Raw upload (with the game's transpose flag) -> column-major matrix.
fn to_colmajor(raw: &[f32; 16], transpose: u8) -> [f32; 16] {
    if transpose == 0 {
        return *raw;
    }
    let mut m = [0.0f32; 16];
    for c in 0..4 {
        for r in 0..4 {
            m[c * 4 + r] = raw[r * 4 + c];
        }
    }
    m
}

fn mat_mul(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut m = [0.0f32; 16];
    for c in 0..4 {
        for r in 0..4 {
            m[c * 4 + r] = (0..4).map(|k| a[k * 4 + r] * b[c * 4 + k]).sum();
        }
    }
    m
}

/// Inverse of an affine column-major matrix (bottom row assumed 0,0,0,1).
fn inv_affine(m: &[f32; 16]) -> Option<[f32; 16]> {
    let (a, b, c) = ([m[0], m[1], m[2]], [m[4], m[5], m[6]], [m[8], m[9], m[10]]);
    let cross = |u: [f32; 3], v: [f32; 3]| {
        [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]]
    };
    let (r0, r1, r2) = (cross(b, c), cross(c, a), cross(a, b));
    let det = a[0] * r0[0] + a[1] * r0[1] + a[2] * r0[2];
    if !det.is_finite() || det.abs() < 1e-12 {
        return None;
    }
    let d = 1.0 / det;
    // Inverse columns are r0/det, r1/det, r2/det as ROWS of the inverse.
    let rows = [r0, r1, r2];
    let t = [m[12], m[13], m[14]];
    let mut o = [0.0f32; 16];
    for (i, row) in rows.iter().enumerate() {
        for j in 0..3 {
            o[j * 4 + i] = row[j] * d;
        }
        o[12 + i] = -(row[0] * t[0] + row[1] * t[1] + row[2] * t[2]) * d;
    }
    o[15] = 1.0;
    Some(o)
}

fn has_body_mats() -> bool {
    let view = ACTIVE_VIEW.load(Ordering::Relaxed);
    BODY_MATS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .any(|e| e.view == view)
}

fn record_body_mat(body: usize, mat: [f32; 16]) {
    let frame = FRAME.load(Ordering::Relaxed);
    let view = ACTIVE_VIEW.load(Ordering::Relaxed);
    let mut mats = BODY_MATS.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(e) = mats.iter_mut().find(|e| e.view == view && e.body == body) {
        if e.frame != frame {
            e.reach2 = body_reach2(body);
        }
        e.frame = frame;
        e.mat = mat;
        return;
    }
    mats.retain(|e| e.frame.saturating_add(1) >= frame);
    if mats.len() < MAX_BODY_MATS {
        mats.push(BodyMat { frame, view, body, mat, reach2: body_reach2(body) });
    }
}

/// Local-space vertex bbox of one draw's index range (min, max), raw floats.
type DrawBox = ([f32; 3], [f32; 3]);
const MAX_BOX_CACHE: usize = 4096;
/// (ibo, first, count) -> bbox (None = unreadable; never retried).
static DRAW_BOXES: LazyLock<Mutex<HashMap<(u32, usize, i32), Option<DrawBox>>>> =
    LazyLock::new(|| Mutex::new(HashMap::default()));
/// (ibo, first, count, candidate-set hash) -> chosen body (matrix-matched draws).
static BODY_CHOICE: LazyLock<Mutex<HashMap<(u32, usize, i32, u64), usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::default()));
static CAND_LOGGED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// GPU readbacks allowed per frame (each glGetBufferSubData stalls the pipeline).
const MAX_READBACKS_PER_FRAME: u32 = 2;
static READBACK_FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
static READBACK_USED: AtomicU32 = AtomicU32::new(0);

fn take_readback_budget() -> bool {
    let frame = FRAME.load(Ordering::Relaxed);
    if READBACK_FRAME.swap(frame, Ordering::Relaxed) != frame {
        READBACK_USED.store(0, Ordering::Relaxed);
    }
    READBACK_USED.fetch_add(1, Ordering::Relaxed) < MAX_READBACKS_PER_FRAME
}

/// Vertex bbox of the draw, read back from the GPU at most once per key.
/// Outer `None` = over this frame's readback budget (nothing cached; retry later).
unsafe fn draw_bbox(
    gl: &'static Gl,
    program: u32,
    ibo: u32,
    count: i32,
    indices: *const c_void,
) -> Option<Option<DrawBox>> {
    let first = indices as usize / 2;
    let key = (ibo, first, count);
    if let Some(v) = DRAW_BOXES.lock().unwrap_or_else(|p| p.into_inner()).get(&key) {
        return Some(*v);
    }
    if !take_readback_budget() {
        return None;
    }
    let started = std::time::Instant::now();
    let found = read_draw_bbox(gl, program, count, first);
    {
        let mut boxes = DRAW_BOXES.lock().unwrap_or_else(|p| p.into_inner());
        if boxes.len() >= MAX_BOX_CACHE {
            boxes.clear();
        }
        boxes.insert(key, found);
    }
    #[cfg(feature = "logging")]
    {
        static LOGGED: AtomicU32 = AtomicU32::new(0);
        if LOGGED.fetch_add(1, Ordering::Relaxed) < 64 {
            log_line(&format!(
                "FLIR: vertex bbox readback ibo={ibo} first={first} count={count}: {found:?} in {} us",
                started.elapsed().as_micros()
            ));
        }
    }
    #[cfg(not(feature = "logging"))]
    let _ = started;
    Some(found)
}

unsafe fn read_draw_bbox(gl: &'static Gl, program: u32, count: i32, first: usize) -> Option<DrawBox> {
    const ARRAY_BUFFER: u32 = 0x8892;
    const ARRAY_BUFFER_BINDING: u32 = 0x8894;
    const ELEMENT_ARRAY_BUFFER: u32 = 0x8893;
    const BUFFER_SIZE: u32 = 0x8764;
    let loc = glq!(gl.get_attrib_location, program, b"vertex_position_in\0".as_ptr() as *const i8);
    if loc < 0 {
        return None;
    }
    let loc = loc as u32;
    let (mut vbo, mut size, mut kind, mut stride) = (0i32, 0i32, 0i32, 0i32);
    glq!(gl.get_vertex_attribiv, loc, 0x889f, &mut vbo);
    glq!(gl.get_vertex_attribiv, loc, 0x8623, &mut size);
    glq!(gl.get_vertex_attribiv, loc, 0x8625, &mut kind);
    glq!(gl.get_vertex_attribiv, loc, 0x8624, &mut stride);
    let mut ptr: *const c_void = std::ptr::null();
    glq!(gl.get_vertex_attrib_pointerv, loc, 0x8645, &mut ptr);
    let offset = ptr as usize;
    if kind as u32 != GL_FLOAT || size < 3 || vbo <= 0 {
        return None;
    }
    let n = (count.max(0) as usize).min(1 << 18);
    let mut ebo_size = 0i32;
    glq!(gl.get_buffer_parameteriv, ELEMENT_ARRAY_BUFFER, BUFFER_SIZE, &mut ebo_size);
    if n == 0 || (first + n) * 2 > ebo_size.max(0) as usize {
        return None;
    }
    let mut idx = vec![0u16; n];
    glq!(gl.get_buffer_sub_data, 
        ELEMENT_ARRAY_BUFFER,
        (first * 2) as isize,
        (n * 2) as isize,
        idx.as_mut_ptr() as *mut c_void,
    );
    let (imin, imax) = idx.iter().fold((u16::MAX, 0u16), |a, &i| (a.0.min(i), a.1.max(i)));
    let step = if stride > 0 { stride as usize } else { size as usize * 4 };
    let span = imax as usize - imin as usize + 1;
    let start = offset + imin as usize * step;
    let want = span * step;
    let mut prev_array = 0i32;
    glq!(gl.get_integerv, ARRAY_BUFFER_BINDING, &mut prev_array);
    (gl.bind_buffer)(ARRAY_BUFFER, vbo as u32);
    let mut bsize = 0i32;
    glq!(gl.get_buffer_parameteriv, ARRAY_BUFFER, BUFFER_SIZE, &mut bsize);
    let ok = start + want <= bsize.max(0) as usize;
    let mut data = vec![0u8; if ok { want } else { 0 }];
    if ok {
        glq!(gl.get_buffer_sub_data, ARRAY_BUFFER, start as isize, want as isize, data.as_mut_ptr() as *mut c_void);
    }
    (gl.bind_buffer)(ARRAY_BUFFER, prev_array as u32);
    if !ok {
        return None;
    }
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for &i in &idx {
        let o = (i as usize - imin as usize) * step;
        for a in 0..3 {
            let f = f32::from_le_bytes([data[o + a * 4], data[o + a * 4 + 1], data[o + a * 4 + 2], data[o + a * 4 + 3]]);
            if !f.is_finite() {
                return None;
            }
            lo[a] = lo[a].min(f);
            hi[a] = hi[a].max(f);
        }
    }
    Some((lo, hi))
}

/// Voxel bbox of `bbox` carried through `m` (column-major, affine).
fn voxel_box(bbox: &DrawBox, m: &[f32; 16]) -> ([i32; 3], [i32; 3]) {
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for c in 0..8 {
        let p = [
            if c & 1 != 0 { bbox.1[0] } else { bbox.0[0] },
            if c & 2 != 0 { bbox.1[1] } else { bbox.0[1] },
            if c & 4 != 0 { bbox.1[2] } else { bbox.0[2] },
        ];
        for a in 0..3 {
            let v = m[a] * p[0] + m[4 + a] * p[1] + m[8 + a] * p[2] + m[12 + a];
            lo[a] = lo[a].min(v);
            hi[a] = hi[a].max(v);
        }
    }
    (
        [0, 1, 2].map(|a| (lo[a] / 0.25 + 0.5).floor() as i32),
        [0, 1, 2].map(|a| (hi[a] / 0.25 + 0.5).floor() as i32),
    )
}

/// Voxels of the vertex box inside the grid box grown by one voxel.
fn box_overlap(vlo: [i32; 3], vhi: [i32; 3], gmin: [i32; 3], dims: [u32; 3]) -> i64 {
    (0..3)
        .map(|a| {
            let lo = vlo[a].max(gmin[a] - 1);
            let hi = vhi[a].min(gmin[a] + dims[a] as i32);
            (hi - lo + 1).max(0) as i64
        })
        .product()
}

struct Cand {
    body: usize,
    min: [i32; 3],
    dims: [u32; 3],
    /// `inverse(body_mat) * comp` (identity for a matrix match).
    to_body: [f32; 16],
    dist: f32,
}

/// Deterministic pick: largest bbox overlap, then nearest grid centre, then
/// lowest body pointer. Never recency. Returns the index into `cands`.
fn pick(cands: &[Cand], bbox: Option<&DrawBox>) -> usize {
    let score = |c: &Cand| -> i64 {
        bbox.map_or(0, |b| {
            let (lo, hi) = voxel_box(b, &c.to_body);
            box_overlap(lo, hi, c.min, c.dims)
        })
    };
    let mut best = 0;
    let mut best_key = (i64::MIN, f32::MAX, usize::MAX);
    for (i, c) in cands.iter().enumerate() {
        let s = score(c);
        let better = s > best_key.0
            || (s == best_key.0 && (c.dist < best_key.1 || (c.dist == best_key.1 && c.body < best_key.2)));
        if better {
            best = i;
            best_key = (s, c.dist, c.body);
        }
    }
    best
}

fn describe(cands: &[Cand], bbox: Option<&DrawBox>) -> String {
    cands
        .iter()
        .map(|c| {
            let ov = bbox.map(|b| {
                let (lo, hi) = voxel_box(b, &c.to_body);
                box_overlap(lo, hi, c.min, c.dims)
            });
            format!("{:#x}(grid {:?}+{:?} overlap {:?})", c.body, c.min, c.dims, ov)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn log_candidates(ibo: u32, what: &str, cands: &[Cand], bbox: Option<&DrawBox>, chosen: Option<usize>) {
    #[cfg(feature = "logging")]
    {
        let mut logged = CAND_LOGGED.lock().unwrap_or_else(|p| p.into_inner());
        if logged.len() >= 256 || logged.contains(&ibo) {
            return;
        }
        logged.push(ibo);
        drop(logged);
        log_line(&format!(
            "FLIR: component ibo={ibo} {what}: bbox={bbox:?} candidates[{}]: {} -> chosen {}",
            cands.len(),
            describe(cands, bbox),
            chosen.map_or("none".to_string(), |i| format!("{:#x}", cands[i].body)),
        ));
    }
    #[cfg(not(feature = "logging"))]
    let _ = (ibo, what, cands, bbox, chosen);
}

/// Find the body for a component/unmapped draw. Returns the body and
/// `inverse(body_mat) * comp_mat`. When several bodies share the draw's matrix
/// (one vehicle frame) or contain its origin, the body whose grid overlaps the
/// draw's vertex bbox the most is chosen; the choice never depends on order.
unsafe fn resolve_component(
    gl: &'static Gl,
    program: u32,
    ibo: u32,
    count: i32,
    indices: *const c_void,
    comp: &[f32; 16],
) -> Option<(usize, [f32; 16])> {
    let view = ACTIVE_VIEW.load(Ordering::Relaxed);
    let first = indices as usize / 2;
    let t = [comp[12], comp[13], comp[14]];
    let close = |a: f32, b: f32| (a - b).abs() <= 1e-3;
    let mut exact: Vec<Cand> = Vec::new();
    let mut near: Vec<Cand> = Vec::new();
    {
        let mats = BODY_MATS.lock().unwrap_or_else(|p| p.into_inner());
        // Locked only once a body is close: most unmapped draws are terrain
        // and props far from every vehicle (~100k draws/s with FLIR on).
        let mut grids = None;
        for e in mats.iter().filter(|e| e.view == view).take(MAX_BODY_MATS) {
            let d = [t[0] - e.mat[12], t[1] - e.mat[13], t[2] - e.mat[14]];
            // Cheap sphere reject (an exact matrix match has d = 0): the body
            // matrix is rigid, so a draw origin farther from the body origin
            // than the grid's farthest corner (+1 voxel) cannot be inside it.
            if d[0] * d[0] + d[1] * d[1] + d[2] * d[2] > e.reach2 {
                continue;
            }
            let grids = grids.get_or_insert_with(|| GRID_TEXTURES.lock().unwrap_or_else(|p| p.into_inner()));
            let Some(grid) = grids.get(&e.body) else { continue };
            let (min, dims) = (grid.min, grid.dims);
            if (0..3).all(|i| close(e.mat[12 + i], t[i])) && (0..12).all(|i| close(e.mat[i], comp[i])) {
                exact.push(Cand { body: e.body, min, dims, to_body: IDENTITY, dist: 0.0 });
                continue;
            }
            if d[0] * d[0] + d[1] * d[1] + d[2] * d[2] > grid_reach2(min, dims) {
                continue;
            }
            let Some(inv) = inv_affine(&e.mat) else { continue };
            let mut dist = 0.0f32;
            let mut inside = true;
            for i in 0..3 {
                let p = (0..3).map(|j| inv[j * 4 + i] * d[j]).sum::<f32>() / 0.25;
                let v = p.round() as i32;
                let (lo, hi) = (min[i], min[i] + dims[i] as i32 - 1);
                inside &= v >= lo - 1 && v <= hi + 1;
                let centre = (lo + hi) as f32 * 0.5;
                dist += (p - centre) * (p - centre);
            }
            if inside {
                near.push(Cand { body: e.body, min, dims, to_body: mat_mul(&inv, comp), dist });
            }
        }
    }
    if !exact.is_empty() {
        exact.sort_by_key(|c| c.body);
        if exact.len() == 1 {
            return Some((exact[0].body, IDENTITY));
        }
        let set = exact.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, c| {
            (h ^ c.body as u64).wrapping_mul(0x100_0000_01b3)
        });
        let key = (ibo, first, count, set);
        let cached = BODY_CHOICE.lock().unwrap_or_else(|p| p.into_inner()).get(&key).copied();
        if let Some(body) = cached.filter(|b| exact.iter().any(|c| c.body == *b)) {
            return Some((body, IDENTITY));
        }
        let Some(bbox) = draw_bbox(gl, program, ibo, count, indices) else {
            return None;
        };
        let chosen = pick(&exact, bbox.as_ref());
        log_candidates(ibo, "matrix-match", &exact, bbox.as_ref(), Some(chosen));
        let body = exact[chosen].body;
        {
            let mut choice = BODY_CHOICE.lock().unwrap_or_else(|p| p.into_inner());
            if choice.len() >= MAX_BOX_CACHE {
                choice.clear();
            }
            choice.insert(key, body);
        }
        return Some((body, IDENTITY));
    }
    if near.is_empty() {
        #[cfg(feature = "logging")]
        {
            // Unresolved: log every body with a grid and where the draw lands in it.
            // Only while the diagnostic can still log: past the cap every new
            // terrain IBO cost a stalling GPU readback (2 per frame, forever).
            {
                let logged = CAND_LOGGED.lock().unwrap_or_else(|p| p.into_inner());
                if logged.len() >= 256 || logged.contains(&ibo) {
                    return None;
                }
            }
            let Some(bbox) = draw_bbox(gl, program, ibo, count, indices) else {
                return None;
            };
            let mats = BODY_MATS.lock().unwrap_or_else(|p| p.into_inner());
            let grids = GRID_TEXTURES.lock().unwrap_or_else(|p| p.into_inner());
            let all: Vec<Cand> = mats
                .iter()
                .filter(|e| e.view == view)
                .filter_map(|e| {
                    let g = grids.get(&e.body)?;
                    let inv = inv_affine(&e.mat)?;
                    Some(Cand { body: e.body, min: g.min, dims: g.dims, to_body: mat_mul(&inv, comp), dist: 0.0 })
                })
                .collect();
            drop((mats, grids));
            log_candidates(ibo, &format!("UNRESOLVED (origin {t:?}) all bodies"), &all, bbox.as_ref(), None);
        }
        return None;
    }
    near.sort_by_key(|c| c.body);
    let chosen = if near.len() == 1 {
        0
    } else {
        let Some(bbox) = draw_bbox(gl, program, ibo, count, indices) else {
            return None;
        };
        let i = pick(&near, bbox.as_ref());
        log_candidates(ibo, "origin-in-grid", &near, bbox.as_ref(), Some(i));
        i
    };
    let c = &near[chosen];
    #[cfg(feature = "logging")]
    {
        let mut logged = COMP_LOGGED.lock().unwrap_or_else(|p| p.into_inner());
        if logged.len() < 32 && !logged.contains(&ibo) {
            logged.push(ibo);
            let voxel = [0, 1, 2].map(|i| (c.to_body[12 + i] / 0.25).round() as i32);
            log_line(&format!(
                "FLIR: component draw ibo={ibo} -> body {:#x} voxel {voxel:?}",
                c.body
            ));
        }
    }
    Some((c.body, c.to_body))
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
static SKINNED: [(AtomicU32, AtomicI32, AtomicU32); SKINNED_SLOTS] =
    [const { (AtomicU32::new(0), AtomicI32::new(-1), AtomicU32::new(0x3f80_0000)) }; SKINNED_SLOTS];
static ORIG_GL_UNIFORM1F: AtomicUsize = AtomicUsize::new(0);
/// Light-factor location to put back after the forwarded skinned draw (-1 none).
static RESTORE_LF: AtomicI32 = AtomicI32::new(-1);
static RESTORE_LF_BITS: AtomicU32 = AtomicU32::new(0);

fn register_skinned(program: u32, light_factor: i32) {
    if light_factor < 0 {
        return;
    }
    for slot in SKINNED.iter() {
        if slot.0.load(Ordering::Relaxed) == program {
            return;
        }
        if slot.0.compare_exchange(0, program, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            slot.1.store(light_factor, Ordering::Relaxed);
            return;
        }
    }
}

fn skinned_slot(program: u32) -> Option<&'static (AtomicU32, AtomicI32, AtomicU32)> {
    SKINNED.iter().find(|s| s.0.load(Ordering::Relaxed) == program)
}

/// `glUniform1f` thunk: remembers the game's own light_factor for skinned
/// programs so the marker can be undone after the draw.
pub(crate) extern "C" fn gl_uniform1f_hook(location: i32, value: f32) {
    let original = ORIG_GL_UNIFORM1F.load(Ordering::Relaxed);
    if original != 0 {
        let f: unsafe extern "system" fn(i32, f32) = unsafe { std::mem::transmute(original) };
        unsafe { f(location, value) };
    }
    {
        let program = TRACK_PROGRAM.load(Ordering::Relaxed);
        if program > 0 {
            record_particle_uniform(program as u32, location, value);
        }
    }
    if SKINNED[0].0.load(Ordering::Relaxed) != 0 {
        let program = TRACK_PROGRAM.load(Ordering::Relaxed);
        if program > 0 {
            if let Some(slot) = skinned_slot(program as u32) {
                if slot.1.load(Ordering::Relaxed) == location {
                    slot.2.store(value.to_bits(), Ordering::Relaxed);
                }
            }
        }
    }
}

/// In a FLIR G-buffer pass, mark a skinned draw as a warm body (set its
/// light_factor); the forwarded game draw keeps its own animation.
unsafe fn mark_skinned_draw(gl: &'static Gl, program: u32) {
    let Some(slot) = skinned_slot(program) else { return };
    let loc = slot.1.load(Ordering::Relaxed);
    if loc < 0 {
        return;
    }
    (gl.uniform1f)(loc, WARM_BODY_MARKER);
    RESTORE_LF_BITS.store(slot.2.load(Ordering::Relaxed), Ordering::Relaxed);
    RESTORE_LF.store(loc, Ordering::Relaxed);
}

/// Undo [`mark_skinned_draw`] after the game's draw (same program bound).
unsafe fn restore_skinned_draw() {
    let loc = RESTORE_LF.swap(-1, Ordering::Relaxed);
    if loc >= 0 {
        if let Some(gl) = gl() {
            (gl.uniform1f)(loc, f32::from_bits(RESTORE_LF_BITS.load(Ordering::Relaxed)));
        }
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

#[derive(Clone, Copy)]
struct PartProg {
    program: u32,
    kind: u32,
    /// Game attribute locations: pos, normal, coord/uv, colour, previous pos.
    attrs: [i32; 5],
    noise_unit: i32,
    /// Game uniform locations of interp_factor / timer (glUniform1f).
    uni_locs: [i32; 2],
}

static PARTS: Mutex<Vec<PartProg>> = Mutex::new(Vec::new());
/// Lock-free program ids of PARTS for the glUniform1f hook.
static PART_IDS: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];
/// Last interp_factor / timer per PARTS slot (f32 bits).
static PART_UNI: [[AtomicU32; 2]; 8] = [const { [const { AtomicU32::new(0) }; 2] }; 8];
/// Built FLIR particle programs per (kind, attrs): (program, vp, world, noise, interp, timer).
type PartGl = (u32, i32, i32, i32, i32, i32, i32);
static PART_PROGRAMS: Mutex<Vec<((u32, [i32; 5]), Option<PartGl>)>> = Mutex::new(Vec::new());

fn register_particle(p: PartProg) {
    let mut parts = PARTS.lock().unwrap_or_else(|e| e.into_inner());
    if parts.len() < 8 && !parts.iter().any(|e| e.program == p.program) {
        PART_IDS[parts.len()].store(p.program, Ordering::Relaxed);
        parts.push(p);
        log_debug(|| format!(
            "FLIR: game program {} is a particle program kind={} (0 smoke, 1 flare, 2 fire) attrs={:?} noise_unit={} uni={:?}",
            p.program, p.kind, p.attrs, p.noise_unit, p.uni_locs
        ));
    }
}

fn particle_slot(program: u32) -> Option<(usize, PartProg)> {
    if PART_IDS[0].load(Ordering::Relaxed) == 0 || !PART_IDS.iter().any(|id| id.load(Ordering::Relaxed) == program) {
        return None;
    }
    PARTS.lock().unwrap_or_else(|e| e.into_inner()).iter().enumerate().find(|(_, e)| e.program == program).map(|(i, e)| (i, *e))
}

/// glUniform1f hook side: remember a particle program's interp_factor/timer.
fn record_particle_uniform(program: u32, location: i32, value: f32) {
    if PART_IDS[0].load(Ordering::Relaxed) == 0 {
        return;
    }
    let Some(i) = PART_IDS.iter().position(|id| id.load(Ordering::Relaxed) == program) else {
        return;
    };
    let Some(p) = PARTS.lock().unwrap_or_else(|e| e.into_inner()).get(i).copied() else {
        return;
    };
    for k in 0..2 {
        if p.uni_locs[k] >= 0 && p.uni_locs[k] == location {
            PART_UNI[i][k].store(value.to_bits(), Ordering::Relaxed);
        }
    }
}

const PART_VS: &str = r#"
in vec3 vertex_position_in;
in vec3 vertex_normal_in;
in vec2 vertex_uv_in;
in vec4 vertex_color_in;
#if KIND == 1 || KIND == 3
in vec4 vertex_bone_weights_in; // flare: previous position (motion blend)
uniform float interp_factor;
#endif
uniform mat4 mat_view_proj;
uniform mat4 mat_world;
out float log_z;
out vec2 uv;
out vec3 puff;
out vec3 col;
void main()
{
    vec3 p = vertex_position_in;
#if KIND == 1 || KIND == 3
    p = mix(vertex_bone_weights_in.xyz, vertex_position_in, interp_factor);
#endif
    uv = vertex_uv_in;
    puff = vertex_normal_in;
#if KIND == 0
    col = vertex_color_in.rgb;
#else
    col = vec3(0.0);
#endif
    gl_Position = mat_view_proj * (mat_world * vec4(p, 1.0));
    log_z = gl_Position.w * 0.01 + 1.0;
}
"#;

// Same cut-outs as smoke_flare_disolve / flare / fire .glslf; the temperature
// excess (C over the air, /1000 in gcolor.r) replaces the colour.
const PART_FS: &str = r#"
in float log_z;
in vec2 uv;
in vec3 puff;
in vec3 col;
uniform sampler2D texture_noise0;
uniform float timer;
uniform float air_c; // the composite adds the excess to this air temperature
out vec4 flir_out0;
out vec4 flir_out1;
out vec4 flir_out2;
void main()
{
    gl_FragDepth = log2(max(log_z, 1e-6)) / 7.658227526203386;
    float excess;
#if KIND == 0
    if (texture(texture_noise0, uv + puff.xy).r > puff.z * 6.0 * (0.5 - length(uv)))
        discard;
    float life = clamp(puff.z, 0.0, 1.0);          // remaining life
    float lum = (col.r + col.g + col.b) / 3.0;
    if (col.r > 0.8 && col.g < 0.4)
        excess = 900.0;                            // lava splash
    else if (col.r > col.g + 0.02)
        excess = max(37.0 - air_c, 0.0);           // blood: body temperature
    else if (lum > 0.4)
        excess = 8.0;                              // water spray / steam
    else
        excess = 10.0 + 390.0 * pow(life, 6.0);       // exhaust / smoke: hot at the outlet, cooling as it drifts
#elif KIND == 1 || KIND == 3
    float nf = texture(texture_noise0, uv + vec2(0.0, timer) * 0.2 + puff.yz).r
             * texture(texture_noise0, uv * 1.4 + vec2(timer, 0.0) * 0.2 + puff.yz).r;
    float df = clamp(2.5 - 2.0 * clamp(length(uv - vec2(0.5)) * 2.0, 0.0, 1.0), 0.0, 1.0);
    if (nf * df <= mix(0.2, 0.6, 1.0 - puff.x))
        discard;
#if KIND == 1
    float heat = 1.0 - clamp((1.0 - puff.x) * 3.0, 0.0, 1.0);
    excess = mix(300.0, 1000.0, heat);             // burning flare / flame
#else
    float life = clamp(puff.x, 0.0, 1.0);          // remaining life (-1: none)
    excess = 10.0 + 490.0 * pow(life, 6.0);           // plume: hot at the nozzle, cools downstream
#endif
#else
    float nf = texture(texture_noise0, uv + vec2(0.0, timer) * 0.5 + puff.yz).r
             * texture(texture_noise0, uv * 1.4 + puff.yz).r;
    float dc = clamp(1.0 - length(uv - vec2(0.5)), 0.0, 1.0);
    float life = clamp(puff.x, 0.0, 1.0);
    if (nf * dc < 0.25 + (1.0 - pow(life, 0.1)) * 0.75)
        discard;
    excess = 300.0 + 600.0 * life;                 // fireball
#endif
    flir_out0 = vec4(0.0, 1.0, 0.0, -5.0);
    flir_out1 = vec4(min(excess / 1000.0, 1.0), 0.0, 0.0, 1.0);
    flir_out2 = vec4(0.0);
}
"#;

/// Whether any attached shader's source contains `needle` (the game prepends
/// its #defines to the source). None = sources unavailable. Setup-time query.
unsafe fn program_defines(gl: &'static Gl, program: u32, needle: &str) -> Option<bool> {
    let mut shaders = [0u32; 4];
    let mut n = 0i32;
    (gl.get_attached_shaders)(program, 4, &mut n, shaders.as_mut_ptr());
    let mut seen = false;
    for &shader in shaders.iter().take(n.clamp(0, 4) as usize) {
        let mut buf = vec![0i8; 65536];
        let mut len = 0i32;
        (gl.get_shader_source)(shader, buf.len() as i32, &mut len, buf.as_mut_ptr());
        if len <= 0 {
            continue;
        }
        seen = true;
        let text: Vec<u8> = buf[..len as usize].iter().map(|&c| c as u8).collect();
        // Tolerate "#define HEAT_ENABLED 1" with any spacing.
        let flat: String = String::from_utf8_lossy(&text).split_whitespace().collect::<Vec<_>>().join(" ");
        if flat.contains(needle) {
            return Some(true);
        }
    }
    seen.then_some(false)
}

unsafe fn particle_program(gl: &'static Gl, p: &PartProg) -> Option<PartGl> {
    let key = (p.kind, p.attrs);
    let mut cache = PART_PROGRAMS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, built)) = cache.iter().find(|e| e.0 == key) {
        return *built;
    }
    let head = format!("#version 330 core\n#define KIND {}\n", p.kind);
    let vs = format!("{head}{PART_VS}");
    let fs = format!("{head}{PART_FS}");
    let attrs = p.attrs;
    let built = link_program(gl, &vs, &fs, |gl, program| {
        let names: [&[u8]; 5] = [
            b"vertex_position_in\0",
            b"vertex_normal_in\0",
            b"vertex_uv_in\0",
            b"vertex_color_in\0",
            b"vertex_bone_weights_in\0",
        ];
        for (loc, name) in attrs.iter().zip(names) {
            if *loc >= 0 {
                (gl.bind_attrib_location)(program, *loc as u32, name.as_ptr() as *const i8);
            }
        }
        (gl.bind_frag_data_location)(program, 0, b"flir_out0\0".as_ptr() as *const i8);
        (gl.bind_frag_data_location)(program, 1, b"flir_out1\0".as_ptr() as *const i8);
        (gl.bind_frag_data_location)(program, 2, b"flir_out2\0".as_ptr() as *const i8);
    })
    .map(|program| {
        (
            program,
            uniform(gl, program, b"mat_view_proj\0"),
            uniform(gl, program, b"mat_world\0"),
            uniform(gl, program, b"texture_noise0\0"),
            uniform(gl, program, b"interp_factor\0"),
            uniform(gl, program, b"timer\0"),
            uniform(gl, program, b"air_c\0"),
        )
    });
    if built.is_none() {
        log_debug(|| format!("FLIR: particle program kind={} failed to build; game draws kept", p.kind));
    }
    if cache.len() < 16 {
        cache.push((key, built));
    }
    built
}

/// Redraw a particle batch as hot gas. False = keep the game draw.
unsafe fn particle_draw(
    gl: &'static Gl,
    slot: usize,
    p: PartProg,
    mode: u32,
    count: i32,
    kind: u32,
    indices: *const c_void,
) -> bool {
    let game = p.program;
    let Some(ProgMats { vp: Some((vp, vpt)), world: Some((w, wt)), .. }) =
        PROG_MATS.lock().unwrap_or_else(|e| e.into_inner()).get(&game).copied()
    else {
        log_once_key(0x6a5_0004, || format!("FLIR particles: program {game} has no recorded matrices; game draw kept"));
        return false;
    };
    let Some((program, vp_loc, world_loc, noise_loc, interp_loc, timer_loc, air_loc)) = particle_program(gl, &p) else {
        return false;
    };
    log_once_key(0x6a5_0005 + p.kind as u64 * 0x100, || {
        format!("FLIR particles: first substituted draw kind={} program={game} count={count}", p.kind)
    });
    IN_SUBSTITUTE.store(true, Ordering::Release);
    let mut guard = StateGuard {
        gl,
        state: Some(SavedState {
            program: game as i32,
            active_texture: tracked(gl, &TRACK_ACTIVE_TEX, GL_ACTIVE_TEXTURE),
        }),
    };
    (gl.use_program)(program);
    (gl.uniform_matrix4fv)(vp_loc, 1, vpt, vp.as_ptr());
    (gl.uniform_matrix4fv)(world_loc, 1, wt, w.as_ptr());
    (gl.uniform1i)(noise_loc, p.noise_unit.max(0));
    if interp_loc >= 0 {
        (gl.uniform1f)(interp_loc, f32::from_bits(PART_UNI[slot][0].load(Ordering::Relaxed)));
    }
    if timer_loc >= 0 {
        (gl.uniform1f)(timer_loc, f32::from_bits(PART_UNI[slot][1].load(Ordering::Relaxed)));
    }
    if air_loc >= 0 {
        // Same air the composite uses for the main view (feeds differ by a
        // degree or two at most).
        (gl.uniform1f)(air_loc, scene_ambient());
    }
    (gl.draw_elements)(mode, count, kind, indices);
    if let Some(state) = guard.state.take() {
        (gl.use_program)(state.program as u32);
    }
    true
}

/// A program is treated as the game's opaque pass (or one of its variants) when
/// it exposes the opaque attribute/uniform layout. Only checked for a draw whose
/// IBO is already mapped to a body, so the query cost is paid once per program.
fn opaque_program(gl: &'static Gl, program: u32) -> bool {
    {
        let programs = OPAQUE_PROGRAMS.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(&known) = programs.get(&program) {
            return known;
        }
    }
    unsafe {
        let pos =
            glq!(gl.get_attrib_location, program, b"vertex_position_in\0".as_ptr() as *const i8);
        let norm =
            glq!(gl.get_attrib_location, program, b"vertex_normal_in\0".as_ptr() as *const i8);
        let world =
            glq!(gl.get_uniform_location, program, b"mat_world\0".as_ptr() as *const i8);
        if pos < 0 || norm < 0 || world < 0 {
            // smoke_flare lands here too (normal optimised out). RE (re_clouds):
            // its only CPU pushers are the weather clouds, so the game draw is
            // kept and clouds shade like the rest of the scene.
            log_rejected_program(gl, program, pos, norm, world);
            remember_opaque(program, false);
            return false;
        }
        // Skinned (characters: bone matrices) and vertex-animated programs move
        // their vertices in the vertex shader; the block VS would draw the bind
        // pose (the T-posing player touching a vehicle). Never substitute them.
        let bones =
            glq!(gl.get_attrib_location, program, b"vertex_bone_weights_in\0".as_ptr() as *const i8);
        let animated =
            glq!(gl.get_attrib_location, program, b"vertex_velocity_in\0".as_ptr() as *const i8);
        // smoke_flare_disolve (exhaust/impact smoke puffs, batched): noise-
        // dissolved billboards whose vertex_normal.z is the puff's remaining
        // life. Redrawn as hot gas, never as vehicle blocks.
        let noise = glq!(gl.get_uniform_location, program, b"texture_noise0\0".as_ptr() as *const i8);
        if noise >= 0 {
            let attr = |n: &[u8]| glq!(gl.get_attrib_location, program, n.as_ptr() as *const i8);
            let uni = |n: &[u8]| glq!(gl.get_uniform_location, program, n.as_ptr() as *const i8);
            let coord = attr(b"vertex_coord_in\0");
            let uvloc = attr(b"vertex_uv_in\0");
            let colour = attr(b"vertex_color_in\0");
            let interp = uni(b"interp_factor\0");
            let timer = uni(b"timer\0");
            // foam_particle (ocean foam) also has noise + timer + uv, plus
            // light_factor/texture_falloff, which fire.glslf lacks.
            let foam = uni(b"light_factor\0") >= 0 || uni(b"texture_falloff\0") >= 0;
            let kind = if coord >= 0 && bones < 0 {
                Some((PK_DISSOLVE, coord))
            } else if bones >= 0 && interp >= 0 && uvloc >= 0 {
                // flare is compiled twice; only the HEAT_ENABLED=1 variant
                // draws flares/flames. The other one draws cool billboards
                // (clouds read white-hot otherwise): keep its game draw.
                // flare is compiled twice: HEAT_ENABLED=1 draws glowing
                // flares/flames, the plain variant smoke-like plumes (jet
                // exhaust): hot, cooling with the particle's life.
                if program_defines(gl, program, "HEAT_ENABLED 1") == Some(false) {
                    Some((PK_PLUME, uvloc))
                } else {
                    Some((PK_FLARE, uvloc))
                }
            } else if bones < 0 && timer >= 0 && uvloc >= 0 && !foam {
                Some((PK_FIRE, uvloc))
            } else {
                None
            };
            if let Some((kind, uv)) = kind {
                // The sampler's unit is assigned once at shader setup (0 in
                // every particle pass per the RE notes).
                let mut unit = 0i32;
                (gl.get_uniformiv)(program, noise, &mut unit);
                register_particle(PartProg {
                    program,
                    kind,
                    attrs: [pos, norm, uv, if kind == PK_DISSOLVE { colour } else { -1 }, if kind == PK_FLARE || kind == PK_PLUME { bones } else { -1 }],
                    noise_unit: unit,
                    uni_locs: [interp, timer],
                });
                remember_opaque(program, false);
                return false;
            }
        }
        if bones >= 0 || animated >= 0 {
            if bones >= 0 {
                let lf = glq!(gl.get_uniform_location, program, b"light_factor\0".as_ptr() as *const i8);
                register_skinned(program, lf);
            }
            log_debug(|| format!("FLIR: game program {program} is skinned/animated; never substituted"));
            remember_opaque(program, false);
            return false;
        }
    }
    remember_opaque(program, true);
    log_debug(|| {
        format!("FLIR: game program {program} cached as an opaque pass (block substitution)")
    });
    true
}

fn remember_opaque(program: u32, opaque: bool) {
    let mut programs = OPAQUE_PROGRAMS.lock().unwrap_or_else(|p| p.into_inner());
    if programs.len() >= 1024 {
        programs.clear();
    }
    programs.insert(program, opaque);
}

/// One-shot per rejected program: which opaque-layout pieces it lacks and its
/// fragment outputs (GL 4.3 program interface, optional entry points).
fn log_rejected_program(_gl: &'static Gl, program: u32, pos: i32, norm: i32, world: i32) {
    #[cfg(feature = "logging")]
    {
        static SEEN: Mutex<Vec<u32>> = Mutex::new(Vec::new());
        {
            let mut seen = SEEN.lock().unwrap_or_else(|p| p.into_inner());
            if seen.len() >= 16 || seen.contains(&program) {
                return;
            }
            seen.push(program);
        }
        let mut outs = String::new();
        unsafe {
            const GL_PROGRAM_OUTPUT: u32 = 0x92e4;
            const GL_ACTIVE_RESOURCES: u32 = 0x92f5;
            let iface = gl_symbol(b"glGetProgramInterfaceiv\0");
            let name_fn = gl_symbol(b"glGetProgramResourceName\0");
            let loc_fn = gl_symbol(b"glGetProgramResourceLocation\0");
            if iface != 0 && name_fn != 0 && loc_fn != 0 {
                let iface: unsafe extern "system" fn(u32, u32, u32, *mut i32) = std::mem::transmute(iface);
                let name_fn: unsafe extern "system" fn(u32, u32, u32, i32, *mut i32, *mut i8) =
                    std::mem::transmute(name_fn);
                let loc_fn: unsafe extern "system" fn(u32, u32, *const i8) -> i32 = std::mem::transmute(loc_fn);
                let mut n = 0i32;
                iface(program, GL_PROGRAM_OUTPUT, GL_ACTIVE_RESOURCES, &mut n);
                for i in 0..n.clamp(0, 16) as u32 {
                    let mut buf = [0i8; 64];
                    let mut len = 0i32;
                    name_fn(program, GL_PROGRAM_OUTPUT, i, 64, &mut len, buf.as_mut_ptr());
                    let loc = loc_fn(program, GL_PROGRAM_OUTPUT, buf.as_ptr());
                    let name: Vec<u8> = buf[..len.clamp(0, 63) as usize].iter().map(|&c| c as u8).collect();
                    outs.push_str(&format!(" {}@{loc}", String::from_utf8_lossy(&name)));
                }
            } else {
                outs.push_str(" (program-interface query unavailable)");
            }
        }
        log_line(&format!(
            "FLIR: program {program} rejected as non-opaque: vertex_position_in={pos} \
vertex_normal_in={norm} mat_world={world}; fragment outputs:{outs}"
        ));
    }
    #[cfg(not(feature = "logging"))]
    let _ = (program, pos, norm, world);
}

/// Restores the GL state [`begin_block_draw`] changed, on the normal path and
/// when the substitute draw panics.
struct StateGuard {
    gl: &'static Gl,
    state: Option<SavedState>,
}

impl Drop for StateGuard {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            unsafe { end_block_draw(self.gl, state) };
        }
        IN_SUBSTITUTE.store(false, Ordering::Release);
    }
}

/// Bind the body temperature texture and the FLIR block program. Returns the
/// saved GL state to restore after the draw, or `None` to fall back.
unsafe fn begin_block_draw(
    gl: &'static Gl,
    body: usize,
    ibo: u32,
    count: i32,
    indices: *const c_void,
    normal_encoded: bool,
    comp_to_body: &[f32; 16],
) -> Option<SavedState> {
    let view_proj = *VIEW_PROJ.lock().unwrap_or_else(|p| p.into_inner());
    let world = *WORLD_MATRIX.lock().unwrap_or_else(|p| p.into_inner());
    let view_proj_transpose = VIEW_PROJ_TRANSPOSE.load(Ordering::Relaxed) as u8;
    let world_transpose = WORLD_TRANSPOSE.load(Ordering::Relaxed) as u8;

    let program = tracked(gl, &TRACK_PROGRAM, GL_CURRENT_PROGRAM);
    if program <= 0 {
        return None;
    }
    let block = block_program(gl, program as u32)?;

    let active = tracked(gl, &TRACK_ACTIVE_TEX, GL_ACTIVE_TEXTURE);
    (gl.active_texture)(GL_TEXTURE0 + FLIR_TEXTURE_UNIT);
    // No game shader samples a 3D texture (no sampler3D in rom/graphics/shaders),
    // so unit 7's 3D binding is ours alone (tracked in LAST_3D, never queried).
    let entry = match body_texture(gl, body) {
        Some(entry) => entry,
        None => {
            diag_count(DIAG_NO_GRID);
            (gl.active_texture)(active as u32);
            return None;
        }
    };
    // One line, written and flushed BEFORE the substitute draw: if the first
    // activation is what kills the game, the log already names the body, grid,
    // texture and program it was about to draw with (`log_line` flushes).
    log_once(LOG_FLIR_BLOCK, || {
        format!(
            "FLIR: block pass active: body={body:#x} ibo={ibo} grid_min={:?} dims={:?} \
texture={} program={} game_program={program} count={count} (substitute draw follows)",
            entry.min, entry.dims, entry.texture, block.program
        )
    });
    #[cfg(feature = "logging")]
    vertex_diag(gl, body, ibo, program as u32, count, indices, entry.min, entry.dims);
    if LAST_3D.swap(entry.texture, Ordering::Relaxed) != entry.texture {
        (gl.bind_texture)(GL_TEXTURE_3D, entry.texture);
    }

    (gl.use_program)(block.program);
    // Component origin in body-local voxels (identity = chunk draw: none).
    let cur = BlockUni {
        vp: (view_proj, view_proj_transpose),
        world: (world, world_transpose),
        comp: *comp_to_body,
        min: entry.min,
        dims: entry.dims,
        normal_encoded,
        comp_voxel: [0, 1, 2].map(|i| (comp_to_body[12 + i] / 0.25).round() as i32),
        is_comp: *comp_to_body != IDENTITY,
    };
    let mut cache = BLOCK_UNI.lock().unwrap_or_else(|p| p.into_inner());
    let slot = cache.iter().position(|e| e.0 == block.program);
    let last = slot.map(|i| cache[i].1);
    let changed = |f: &dyn Fn(&BlockUni) -> bool| last.as_ref().map_or(true, |l| !f(l));
    if last.is_none() {
        (gl.uniform1i)(block.tex_temps, FLIR_TEXTURE_UNIT as i32);
        (gl.uniform1f)(block.enc_min, FLIR_ENC_MIN_C);
        (gl.uniform1f)(block.enc_max, FLIR_ENC_MAX_C);
        if block.has_color >= 0 {
            (gl.uniform1i)(block.has_color, (block.color_loc >= 0) as i32);
        }
    }
    // The matrices are replayed exactly as the game uploaded them (raw floats
    // and the game's transpose flag), so the transform is identical.
    if changed(&|l| l.vp == cur.vp) {
        (gl.uniform_matrix4fv)(block.mat_view_proj, 1, view_proj_transpose, view_proj.as_ptr());
    }
    if changed(&|l| l.world == cur.world) {
        (gl.uniform_matrix4fv)(block.mat_world, 1, world_transpose, world.as_ptr());
    }
    if changed(&|l| l.comp == cur.comp) {
        (gl.uniform_matrix4fv)(block.comp_to_body, 1, 0, comp_to_body.as_ptr());
    }
    if changed(&|l| l.min == cur.min) {
        (gl.uniform3i)(block.grid_min, entry.min[0], entry.min[1], entry.min[2]);
    }
    if changed(&|l| l.dims == cur.dims) {
        (gl.uniform3i)(block.grid_dims, entry.dims[0] as i32, entry.dims[1] as i32, entry.dims[2] as i32);
    }
    // The attachment's component type decides the normal packing and which
    // channel carries the vehicle marker (see BLOCK_FS).
    if changed(&|l| l.normal_encoded == cur.normal_encoded) {
        (gl.uniform1i)(block.normal_encoded, normal_encoded as i32);
    }
    if block.comp_voxel >= 0 && block.has_comp_voxel >= 0 {
        if changed(&|l| l.comp_voxel == cur.comp_voxel) {
            let v = cur.comp_voxel;
            (gl.uniform3i)(block.comp_voxel, v[0], v[1], v[2]);
        }
        if changed(&|l| l.is_comp == cur.is_comp) {
            (gl.uniform1i)(block.has_comp_voxel, cur.is_comp as i32);
        }
    }
    match slot {
        Some(i) => cache[i].1 = cur,
        None => cache.push((block.program, cur)),
    }

    Some(SavedState {
        program,
        active_texture: active,
    })
}

/// One-shot per body: read back the vertices the draw really uses and compare
/// their voxel coordinates (shader formula, normal ignored) with the grid.
#[cfg(feature = "logging")]
#[allow(clippy::too_many_arguments)]
unsafe fn vertex_diag(
    gl: &'static Gl,
    body: usize,
    ibo: u32,
    program: u32,
    count: i32,
    indices: *const c_void,
    gmin: [i32; 3],
    dims: [u32; 3],
) {
    static DONE: Mutex<Vec<usize>> = Mutex::new(Vec::new());
    {
        let mut done = DONE.lock().unwrap_or_else(|p| p.into_inner());
        if done.len() >= 16 || done.contains(&body) {
            return;
        }
        done.push(body);
    }
    const ARRAY_BUFFER: u32 = 0x8892;
    const ARRAY_BUFFER_BINDING: u32 = 0x8894;
    const ELEMENT_ARRAY_BUFFER: u32 = 0x8893;
    const BUFFER_SIZE: u32 = 0x8764;
    let loc = glq!(gl.get_attrib_location, program, b"vertex_position_in\0".as_ptr() as *const i8);
    if loc < 0 {
        return;
    }
    let loc = loc as u32;
    let (mut vbo, mut size, mut kind, mut stride, mut norm) = (0i32, 0i32, 0i32, 0i32, 0i32);
    glq!(gl.get_vertex_attribiv, loc, 0x889f, &mut vbo);
    glq!(gl.get_vertex_attribiv, loc, 0x8623, &mut size);
    glq!(gl.get_vertex_attribiv, loc, 0x8625, &mut kind);
    glq!(gl.get_vertex_attribiv, loc, 0x8624, &mut stride);
    glq!(gl.get_vertex_attribiv, loc, 0x886a, &mut norm);
    let mut ptr: *const c_void = std::ptr::null();
    glq!(gl.get_vertex_attrib_pointerv, loc, 0x8645, &mut ptr);
    let offset = ptr as usize;
    let first = indices as usize / 2;
    let head = format!(
        "FLIR diag: body={body:#x} ibo={ibo} vbo={vbo} attrib(loc={loc} size={size} type={kind:#x} \
stride={stride} normalized={norm} offset={offset}) draw first={first} count={count} \
grid_min={gmin:?} dims={dims:?}"
    );
    if kind as u32 != GL_FLOAT || size < 3 || vbo <= 0 {
        log_line(&format!("{head}: vertex type not float3+, skipped"));
        return;
    }
    // Sample indices from the element buffer (already bound by the game).
    let n = (count.max(0) as usize).min(512);
    let mut idx = vec![0u16; n];
    glq!(gl.get_buffer_sub_data, ELEMENT_ARRAY_BUFFER, (first * 2) as isize, (n * 2) as isize, idx.as_mut_ptr() as *mut c_void);
    let (imin, imax) = idx.iter().fold((u16::MAX, 0u16), |a, &i| (a.0.min(i), a.1.max(i)));
    let step = if stride > 0 { stride as usize } else { size as usize * 4 };
    let mut prev_array = 0i32;
    glq!(gl.get_integerv, ARRAY_BUFFER_BINDING, &mut prev_array);
    (gl.bind_buffer)(ARRAY_BUFFER, vbo as u32);
    let mut bsize = 0i32;
    glq!(gl.get_buffer_parameteriv, ARRAY_BUFFER, BUFFER_SIZE, &mut bsize);
    let span = ((imax as usize).saturating_sub(imin as usize) + 1).min(2048);
    let start = offset + imin as usize * step;
    let want = span * step;
    let mut data = vec![0u8; want];
    let ok = n > 0 && start + want <= bsize.max(0) as usize;
    if ok {
        glq!(gl.get_buffer_sub_data, ARRAY_BUFFER, start as isize, want as isize, data.as_mut_ptr() as *mut c_void);
    }
    (gl.bind_buffer)(ARRAY_BUFFER, prev_array as u32);
    if !ok {
        log_line(&format!(
            "{head}: indices {imin}..{imax} need bytes {start}+{want} but vbo size={bsize}; skipped"
        ));
        return;
    }
    let mut vox: Vec<[i32; 3]> = Vec::new();
    let (mut raw_min, mut raw_max) = ([f32::MAX; 3], [f32::MIN; 3]);
    for &i in &idx {
        let rel = (i as usize).wrapping_sub(imin as usize);
        if rel >= span {
            continue;
        }
        let o = rel * step;
        if o + 12 > data.len() {
            continue;
        }
        let mut v = [0i32; 3];
        for a in 0..3 {
            let f = f32::from_le_bytes([data[o + a * 4], data[o + a * 4 + 1], data[o + a * 4 + 2], data[o + a * 4 + 3]]);
            raw_min[a] = raw_min[a].min(f);
            raw_max[a] = raw_max[a].max(f);
            v[a] = (f / 0.25 + 0.5).floor() as i32;
        }
        vox.push(v);
    }
    if vox.is_empty() {
        log_line(&format!("{head}: no vertices decoded"));
        return;
    }
    let (mut vmin, mut vmax) = ([i32::MAX; 3], [i32::MIN; 3]);
    for v in &vox {
        for a in 0..3 {
            vmin[a] = vmin[a].min(v[a]);
            vmax[a] = vmax[a].max(v[a]);
        }
    }
    let gmax = [
        gmin[0] + dims[0] as i32 - 1,
        gmin[1] + dims[1] as i32 - 1,
        gmin[2] + dims[2] as i32 - 1,
    ];
    let inside = |perm: [usize; 3], sign: [i32; 3]| {
        vox.iter()
            .filter(|v| {
                (0..3).all(|a| {
                    let c = sign[a] * v[perm[a]];
                    c >= gmin[a] && c <= gmax[a]
                })
            })
            .count()
    };
    let perms = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]];
    let mut fits = String::new();
    let mut best = (0usize, [0usize; 3], [1i32; 3]);
    for perm in perms {
        for bits in 0..8u32 {
            let sign = [
                if bits & 1 != 0 { -1 } else { 1 },
                if bits & 2 != 0 { -1 } else { 1 },
                if bits & 4 != 0 { -1 } else { 1 },
            ];
            let c = inside(perm, sign);
            if c > best.0 {
                best = (c, perm, sign);
            }
            if c == vox.len() && fits.len() < 200 {
                fits.push_str(&format!(" perm={perm:?} sign={sign:?}"));
            }
        }
    }
    log_line(&format!(
        "{head}: indices sampled={n} range {imin}..{imax} (span {span}); vertices={} \
raw_pos min={raw_min:?} max={raw_max:?}; voxel min={vmin:?} max={vmax:?} grid max={gmax:?}; \
identity in-grid {}/{}; full-fit transforms:{}; best {}/{} perm={:?} sign={:?}",
        vox.len(),
        inside([0, 1, 2], [1, 1, 1]),
        vox.len(),
        if fits.is_empty() { " none".to_string() } else { fits },
        best.0,
        vox.len(),
        best.1,
        best.2
    ));
}

unsafe fn end_block_draw(gl: &'static Gl, state: SavedState) {
    (gl.active_texture)(state.active_texture as u32);
    (gl.use_program)(state.program as u32);
}

/// Build (or reuse) the FLIR opaque-program copy for the game program `game`.
/// Attribute locations are per program, so the program id is the cache key and
/// a hit costs no GL queries. A failed build is cached as `None`: never retry it
/// (and its info log) on every draw.
unsafe fn block_program(gl: &'static Gl, game: u32) -> Option<BlockProgram> {
    {
        let programs = BLOCK_PROGRAMS.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((_, cached)) = programs.iter().find(|(key, _)| *key == game) {
            return *cached;
        }
        // Every entry owns a linked GL program: past the cap, leave further game
        // programs to the game instead of linking one per draw (the old cap of 8
        // without caching re-linked on every draw of a ninth program).
        if programs.len() >= 32 {
            return None;
        }
    }
    let pos_loc =
        glq!(gl.get_attrib_location, game, b"vertex_position_in\0".as_ptr() as *const i8);
    let norm_loc =
        glq!(gl.get_attrib_location, game, b"vertex_normal_in\0".as_ptr() as *const i8);
    let color_loc =
        glq!(gl.get_attrib_location, game, b"vertex_color_in\0".as_ptr() as *const i8);
    let built = if pos_loc < 0 || norm_loc < 0 {
        log_once(LOG_FLIR_NO_ATTRIB, || {
            format!(
                "FLIR: game program {game} lacks vertex_position_in/vertex_normal_in; \
block pass disabled for it"
            )
        });
        None
    } else {
        build_block_program(gl, (pos_loc, norm_loc), color_loc)
    };
    let mut programs = BLOCK_PROGRAMS.lock().unwrap_or_else(|p| p.into_inner());
    programs.push((game, built));
    built
}

unsafe fn build_block_program(gl: &'static Gl, locations: (i32, i32), color_loc: i32) -> Option<BlockProgram> {
    let program = link_program(gl, BLOCK_VS, BLOCK_FS, |gl, program| {
        (gl.bind_attrib_location)(
            program,
            locations.0 as u32,
            b"vertex_position_in\0".as_ptr() as *const i8,
        );
        (gl.bind_attrib_location)(
            program,
            locations.1 as u32,
            b"vertex_normal_in\0".as_ptr() as *const i8,
        );
        if color_loc >= 0 {
            (gl.bind_attrib_location)(
                program,
                color_loc as u32,
                b"vertex_color_in\0".as_ptr() as *const i8,
            );
        }
        (gl.bind_frag_data_location)(program, 0, b"flir_out0\0".as_ptr() as *const i8);
        (gl.bind_frag_data_location)(program, 1, b"flir_out1\0".as_ptr() as *const i8);
        (gl.bind_frag_data_location)(program, 2, b"flir_out2\0".as_ptr() as *const i8);
    })?;

    let block = BlockProgram {
        program,
        mat_view_proj: uniform(gl, program, b"mat_view_proj\0"),
        mat_world: uniform(gl, program, b"mat_world\0"),
        comp_to_body: uniform(gl, program, b"comp_to_body\0"),
        grid_min: uniform(gl, program, b"grid_min\0"),
        grid_dims: uniform(gl, program, b"grid_dims\0"),
        enc_min: uniform(gl, program, b"enc_min\0"),
        enc_max: uniform(gl, program, b"enc_max\0"),
        tex_temps: uniform(gl, program, b"tex_temps\0"),
        normal_encoded: uniform(gl, program, b"normal_encoded\0"),
        comp_voxel: uniform(gl, program, b"comp_voxel\0"),
        has_color: uniform(gl, program, b"has_color\0"),
        color_loc,
        has_comp_voxel: uniform(gl, program, b"has_comp_voxel\0"),
    };
    if block.mat_view_proj < 0
        || block.mat_world < 0
        || block.comp_to_body < 0
        || block.grid_min < 0
        || block.grid_dims < 0
        || block.enc_min < 0
        || block.enc_max < 0
        || block.tex_temps < 0
        || block.normal_encoded < 0
    {
        log_once(LOG_FLIR_SHADER, || {
            "FLIR: block program is missing a required uniform; block pass disabled".to_string()
        });
        (gl.delete_program)(program);
        return None;
    }
    log_debug(|| {
        format!("FLIR: block program linked (program {program}, attrib locations {locations:?})")
    });
    Some(block)
}

/// Save, zero and restore the unpack state around an upload. A non-default
/// `GL_UNPACK_ROW_LENGTH`/`SKIP_*`/`IMAGE_HEIGHT` or a bound
/// `GL_PIXEL_UNPACK_BUFFER` makes the driver read outside our staging slice
/// (a native fault no vectored handler can catch under Proton). Saved with the
/// client attribute stack and the tracked PBO binding: the old 7 glGetIntegerv
/// per upload were the bulk of the plugin's driver syncs.
struct UnpackGuard {
    gl: &'static Gl,
    pbo: i32,
}

impl UnpackGuard {
    unsafe fn begin(gl: &'static Gl) -> UnpackGuard {
        (gl.push_client_attrib)(GL_CLIENT_PIXEL_STORE_BIT);
        let pbo = tracked(gl, &TRACK_UNPACK_PBO, GL_PIXEL_UNPACK_BUFFER_BINDING);
        if pbo != 0 {
            (gl.bind_buffer)(GL_PIXEL_UNPACK_BUFFER, 0);
        }
        (gl.pixel_storei)(GL_UNPACK_ALIGNMENT, 1);
        (gl.pixel_storei)(GL_UNPACK_ROW_LENGTH, 0);
        (gl.pixel_storei)(GL_UNPACK_IMAGE_HEIGHT, 0);
        (gl.pixel_storei)(GL_UNPACK_SKIP_ROWS, 0);
        (gl.pixel_storei)(GL_UNPACK_SKIP_PIXELS, 0);
        (gl.pixel_storei)(GL_UNPACK_SKIP_IMAGES, 0);
        UnpackGuard { gl, pbo }
    }
}

impl Drop for UnpackGuard {
    fn drop(&mut self) {
        unsafe {
            (self.gl.pop_client_attrib)();
            if self.pbo != 0 {
                (self.gl.bind_buffer)(GL_PIXEL_UNPACK_BUFFER, self.pbo as u32);
            }
        }
    }
}

/// Bind this body's R32F volume, uploading the grid when its version changed
/// (throttled to one upload per body per `FLIR_UPLOAD_INTERVAL_FRAMES`, so a
/// body that warms up continuously does not re-upload every frame). The returned
/// entry describes the texture that is resident (`min`/`dims`), so the shader's
/// texel math can never disagree with the uploaded data.
unsafe fn body_texture(gl: &'static Gl, body: usize) -> Option<GridTexture> {
    let frame = FRAME.load(Ordering::Relaxed);
    let cached = GRID_TEXTURES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&body)
        .copied();
    if let Some(entry) = cached {
        if frame < entry.frame.saturating_add(FLIR_UPLOAD_INTERVAL_FRAMES) {
            return Some(entry);
        }
    }

    // Cheap metadata read: nothing is copied unless the grid actually changed.
    let (version, dims, min) =
        crate::blocktemp::with_body_grid(body, |grid| (grid.version, grid.dims, grid.min))?;
    if let Some(entry) = cached {
        if entry.version == version && entry.dims == dims {
            let mut textures = GRID_TEXTURES.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(stored) = textures.get_mut(&body) {
                stored.frame = frame;
            }
            return Some(GridTexture { frame, ..entry });
        }
    }

    // Bound the dims BEFORE multiplying: a bogus grid must not overflow (a
    // debug-build panic here would unwind through the GL draw thunk). The sim's
    // own grid cap is the single source of truth for the limit.
    if dims.iter().any(|d| *d == 0 || *d > crate::blocktemp::MAX_GRID_DIM) {
        return None;
    }
    let expected = (dims[0] as usize) * (dims[1] as usize) * (dims[2] as usize);
    if expected == 0 {
        return None;
    }

    // Copy the grid into our own staging buffer while it is borrowed, so the
    // sim's published grid is never cloned and the GL call runs lock-free. The
    // render thread is the buffer's only user, so holding it across the upload
    // cannot block the game thread. Re-checking version/dims inside the same
    // borrow keeps the texture and `min`/`dims` consistent.
    let mut staging = UPLOAD_BUF.lock().unwrap_or_else(|p| p.into_inner());
    let copied = crate::blocktemp::with_body_grid(body, |grid| {
        if grid.version != version || grid.dims != dims || grid.temps_c.len() < expected {
            return false;
        }
        staging.clear();
        staging.extend_from_slice(&grid.temps_c[..expected]);
        true
    })
    .unwrap_or(false);
    if !copied {
        log_once(LOG_FLIR_GRID_MISMATCH, || {
            format!(
                "FLIR: body {body:#x} grid dims {dims:?} do not match {expected} temperatures; \
block pass skipped"
            )
        });
        return None;
    }

    // Drop textures no view has used for two frames (never the one we are about
    // to draw with); the ids go out of the lock and are deleted after unbinding.
    let stale: Vec<u32> = {
        let mut textures = GRID_TEXTURES.lock().unwrap_or_else(|p| p.into_inner());
        if textures.len() <= FLIR_GRID_TEXTURE_MAX {
            Vec::new()
        } else {
            let keep: Vec<usize> = textures
                .iter()
                .filter(|(key, entry)| {
                    **key != body && entry.frame.saturating_add(2) < frame
                })
                .map(|(key, _)| *key)
                .collect();
            keep.iter()
                .filter_map(|key| textures.remove(key))
                .map(|entry| entry.texture)
                .collect()
        }
    };
    if !stale.is_empty() {
        (gl.bind_texture)(GL_TEXTURE_3D, 0);
        LAST_3D.store(0, Ordering::Relaxed);
        (gl.delete_textures)(stale.len() as i32, stale.as_ptr());
        log_debug(|| format!("FLIR: evicted {} stale body textures", stale.len()));
    }

    let texture = {
        let textures = GRID_TEXTURES.lock().unwrap_or_else(|p| p.into_inner());
        match textures.get(&body) {
            Some(entry) => entry.texture,
            None => {
                let mut id = 0u32;
                (gl.gen_textures)(1, &mut id);
                if id == 0 {
                    return None;
                }
                id
            }
        }
    };
    (gl.bind_texture)(GL_TEXTURE_3D, texture);
    LAST_3D.store(texture, Ordering::Relaxed);
    {
        let _unpack = UnpackGuard::begin(gl);
        // Same volume size as last time: overwrite the texels in place instead
        // of reallocating the texture storage for every upload.
        if cached.is_some_and(|entry| entry.dims == dims) {
            (gl.tex_sub_image_3d)(
                GL_TEXTURE_3D,
                0,
                0,
                0,
                0,
                dims[0] as i32,
                dims[1] as i32,
                dims[2] as i32,
                GL_RED,
                GL_FLOAT,
                staging.as_ptr() as *const c_void,
            );
        } else {
            (gl.tex_parameteri)(GL_TEXTURE_3D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            (gl.tex_parameteri)(GL_TEXTURE_3D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            (gl.tex_parameteri)(GL_TEXTURE_3D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            (gl.tex_parameteri)(GL_TEXTURE_3D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
            (gl.tex_parameteri)(GL_TEXTURE_3D, GL_TEXTURE_WRAP_R, GL_CLAMP_TO_EDGE);
            (gl.tex_image_3d)(
                GL_TEXTURE_3D,
                0,
                GL_R32F as i32,
                dims[0] as i32,
                dims[1] as i32,
                dims[2] as i32,
                0,
                GL_RED,
                GL_FLOAT,
                staging.as_ptr() as *const c_void,
            );
        }
    }
    let entry = GridTexture {
        texture,
        version,
        min,
        dims,
        frame,
    };
    GRID_TEXTURES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(body, entry);
    Some(entry)
}

/// Describe `fbo` as a G-buffer, or return `None` when it is not one (no two
/// colour textures). Known framebuffers come from the registry without any GL
/// query, so a pass that binds an unknown target pays one probe and nothing
/// after that.
unsafe fn gbuffer_for(gl: &'static Gl, fbo: u32) -> Option<Gbuffer> {
    if fbo == 0 {
        return None;
    }
    let cached = {
        let registry = GBUFFERS.lock().unwrap_or_else(|p| p.into_inner());
        registry.get(&fbo).copied()
    };
    let rejected = cached.is_none() && {
        let rejected = NOT_GBUFFERS.lock().unwrap_or_else(|p| p.into_inner());
        rejected.contains(&fbo)
    };
    match cached {
        Some(entry) => Some(entry),
        None if rejected => None,
        None => match capture_gbuffer(gl, fbo) {
            Some(entry) => Some(entry),
            None => {
                let mut rejected = NOT_GBUFFERS.lock().unwrap_or_else(|p| p.into_inner());
                if rejected.len() >= NOT_GBUFFER_LIMIT {
                    rejected.clear();
                }
                rejected.insert(fbo);
                None
            }
        },
    }
}

/// Probe `fbo`'s attachments: 0 = `gnormal_light_factor`, 1 = `gcolor`, plus
/// depth (RE: the `glDrawBuffers` list is the fixed COLOR_ATTACHMENT0..4 and the
/// opaque shader declares its outputs in that order). Anything without two
/// colour attachments (tonemap, shadow, bloom) is not a G-buffer.
unsafe fn capture_gbuffer(gl: &'static Gl, fbo: u32) -> Option<Gbuffer> {
    // Query the attachments of `fbo` (it is the bound draw framebuffer at every
    // call site).
    let normal = attachment_texture(gl, GL_COLOR_ATTACHMENT0).unwrap_or(0);
    let color = attachment_texture(gl, GL_COLOR_ATTACHMENT0 + 1).unwrap_or(0);
    if normal == 0 || color == 0 {
        return None;
    }
    let depth = attachment_texture(gl, GL_DEPTH_ATTACHMENT)
        .or_else(|| attachment_texture(gl, GL_DEPTH_STENCIL_ATTACHMENT))
        .unwrap_or(0);
    let normal_type = attachment_component_type(gl, GL_COLOR_ATTACHMENT0);
    let color_type = attachment_component_type(gl, GL_COLOR_ATTACHMENT0 + 1);
    // Raw normals in a normalized attachment cannot hold negative components, so
    // the composite then decodes packed xy and moves the vehicle marker to
    // gcolor alpha instead of the light-factor channel.
    let normal_encoded = normal_type != GL_FLOAT as i32;
    let normal_format = attachment_internal_format(gl, normal);
    let color_format = attachment_internal_format(gl, color);
    let normal_size = texture_size(gl, normal);
    let entry = Gbuffer {
        fbo,
        tex_normal: normal,
        tex_color: color,
        depth,
        size: normal_size,
        normal_encoded,
    };
    {
        let mut registry = GBUFFERS.lock().unwrap_or_else(|p| p.into_inner());
        // A long session can cycle resolution targets; keep the registry small
        // rather than letting it grow without bound (a cleared entry is simply
        // probed again on its next bind).
        if registry.len() >= 8 && !registry.contains_key(&fbo) {
            registry.clear();
        }
        registry.insert(fbo, entry);
    }
    log_once_key(0x06a5_8000 | (fbo as u64 & 0x3fff), || {
        format!(
            "FLIR: G-buffer captured from FBO {fbo} ({}x{}): normal(tex {normal}, internal format \
{normal_format:#x}, component type {normal_type:#x}, encoded={normal_encoded}), colour(tex {color}, \
internal format {color_format:#x}, component type {color_type:#x}), depth={depth} \
(0 = renderbuffer attachment)",
            normal_size[0], normal_size[1]
        )
    });
    Some(entry)
}

/// `GL_TEXTURE_INTERNAL_FORMAT` of an attachment texture (0 when unknown). The
/// storage format decides whether the float light-factor marker and raw normals
/// survive, so it is probed (and logged) once per captured G-buffer.
unsafe fn attachment_internal_format(gl: &'static Gl, texture: u32) -> i32 {
    if texture == 0 {
        return 0;
    }
    // glGetTexLevelParameteriv reads the texture bound to GL_TEXTURE_2D on the
    // active unit, so bind ours and put the game's binding back.
    let mut previous = 0i32;
    glq!(gl.get_integerv, GL_TEXTURE_BINDING_2D, &mut previous);
    (gl.bind_texture)(GL_TEXTURE_2D, texture);
    let mut format = 0i32;
    glq!(gl.get_tex_level_parameteriv, GL_TEXTURE_2D, 0, GL_TEXTURE_INTERNAL_FORMAT, &mut format);
    (gl.bind_texture)(GL_TEXTURE_2D, previous as u32);
    format
}

/// Texel size of an attachment texture (`[0, 0]` when unknown). Used to bind a
/// G-buffer to the view whose tonemap viewport has the same size.
unsafe fn texture_size(gl: &'static Gl, texture: u32) -> [i32; 2] {
    if texture == 0 {
        return [0, 0];
    }
    let mut previous = 0i32;
    glq!(gl.get_integerv, GL_TEXTURE_BINDING_2D, &mut previous);
    (gl.bind_texture)(GL_TEXTURE_2D, texture);
    let mut width = 0i32;
    let mut height = 0i32;
    glq!(gl.get_tex_level_parameteriv, GL_TEXTURE_2D, 0, GL_TEXTURE_WIDTH, &mut width);
    glq!(gl.get_tex_level_parameteriv, GL_TEXTURE_2D, 0, GL_TEXTURE_HEIGHT, &mut height);
    (gl.bind_texture)(GL_TEXTURE_2D, previous as u32);
    [width, height]
}

/// `GL_FRAMEBUFFER_ATTACHMENT_COMPONENT_TYPE` of an attachment (0 when unbound).
unsafe fn attachment_component_type(gl: &'static Gl, attachment: u32) -> i32 {
    let mut kind = GL_NONE;
    glq!(gl.get_framebuffer_attachment_parameteriv, 
        GL_FRAMEBUFFER,
        attachment,
        GL_FRAMEBUFFER_ATTACHMENT_COMPONENT_TYPE,
        &mut kind,
    );
    kind
}

unsafe fn attachment_texture(gl: &'static Gl, attachment: u32) -> Option<u32> {
    let mut kind = GL_NONE;
    glq!(gl.get_framebuffer_attachment_parameteriv, 
        GL_FRAMEBUFFER,
        attachment,
        GL_FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE,
        &mut kind,
    );
    if kind != GL_TEXTURE as i32 {
        return None;
    }
    let mut id = 0i32;
    glq!(gl.get_framebuffer_attachment_parameteriv, 
        GL_FRAMEBUFFER,
        attachment,
        GL_FRAMEBUFFER_ATTACHMENT_OBJECT_NAME,
        &mut id,
    );
    if id > 0 {
        Some(id as u32)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Composite pass
// ---------------------------------------------------------------------------

/// GL state the composite changes, restored by [`CompositeGuard`]: enables,
/// colour/depth masks and units 0-2 texture bindings (plus the active unit) go
/// through the compat attribute stack, the program through the tracker - no
/// driver query. The composite never binds a VAO (it draws the game's quad).
struct CompositeGuard {
    gl: &'static Gl,
    program: i32,
}

const COMPOSITE_ATTRIB_BITS: u32 =
    GL_ENABLE_BIT | GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT | GL_TEXTURE_BIT;

impl CompositeGuard {
    unsafe fn begin(gl: &'static Gl) -> CompositeGuard {
        let program = tracked(gl, &TRACK_PROGRAM, GL_CURRENT_PROGRAM);
        (gl.push_attrib)(COMPOSITE_ATTRIB_BITS);
        (gl.disable)(GL_DEPTH_TEST);
        (gl.disable)(GL_BLEND);
        (gl.disable)(GL_CULL_FACE);
        (gl.color_mask)(1, 1, 1, 1);
        (gl.depth_mask)(0);
        CompositeGuard { gl, program }
    }
}

impl Drop for CompositeGuard {
    fn drop(&mut self) {
        unsafe {
            (self.gl.pop_attrib)();
            (self.gl.use_program)(self.program as u32);
        }
    }
}

/// Viewport size last seen per tonemap OUTPUT target. Keyed by the target, not
/// by the tonemap-pass ordinal: one frame renders the main view and a camera
/// feed at different sizes, which is not a resolution change.
const VIEW_SIZE_TARGETS: usize = 8;
static VIEW_SIZE: LazyLock<Mutex<HashMap<u32, [i32; 2]>>> =
    LazyLock::new(|| Mutex::new(HashMap::default()));

/// True when this target's size differs from the last time it was composited.
/// The target-keyed caches are then dropped: after a window or resolution change
/// the game deletes and recreates its render targets and the driver reuses the
/// names, so a cached entry would describe a target that no longer exists - or a
/// recycled id would stay on the rejected list and keep FLIR dead.
/// `MESH_BODY` is deliberately NOT cleared here: it is keyed by mesh index-buffer
/// ids (which a view resize does not touch) and is rebuilt on the game thread
/// every 250 ms, so clearing it from the render thread can only empty a freshly
/// published map.
fn forget_caches_on_resize(target: u32, width: i32, height: i32) -> bool {
    let previous = {
        let mut sizes = VIEW_SIZE.lock().unwrap_or_else(|p| p.into_inner());
        if sizes.len() >= VIEW_SIZE_TARGETS && !sizes.contains_key(&target) {
            sizes.clear();
        }
        sizes.insert(target, [width, height])
    };
    let Some(previous) = previous else {
        return false;
    };
    if previous == [width, height] || previous == [0, 0] {
        return false;
    }
    GBUFFERS.lock().unwrap_or_else(|p| p.into_inner()).clear();
    NOT_GBUFFERS.lock().unwrap_or_else(|p| p.into_inner()).clear();
    // The target/G-buffer ids are recycled as well, so every target fact and
    // pairing is dropped: a G-buffer is only paired again at a tonemap quad.
    {
        let mut targets = TARGETS.lock().unwrap_or_else(|p| p.into_inner());
        targets.by_fbo.clear();
        targets.gbuf_target.clear();
        targets.target_gbuf.clear();
    }
    refresh_forced_gbuffers();
    log_debug(|| {
        format!(
            "FLIR: tonemap target {target} is now {width}x{height} (was {}x{}); G-buffer, \
target-pairing and rejected-FBO caches dropped (the mesh->body map is kept)",
            previous[0], previous[1]
        )
    });
    true
}

/// The tonemap program's quad inputs (position, coord0, mat_view_proj), cached
/// per program: attribute/uniform locations never change after linking.
unsafe fn quad_locs(gl: &'static Gl, program: u32) -> (i32, i32, i32) {
    static LOCS: Mutex<Vec<(u32, (i32, i32, i32))>> = Mutex::new(Vec::new());
    if let Some(&(_, l)) = LOCS.lock().unwrap_or_else(|p| p.into_inner()).iter().find(|e| e.0 == program) {
        return l;
    }
    let l = (
        glq!(gl.get_attrib_location, program, b"vertex_position_in\0".as_ptr() as *const i8),
        glq!(gl.get_attrib_location, program, b"vertex_coord0_in\0".as_ptr() as *const i8),
        glq!(gl.get_uniform_location, program, b"mat_view_proj\0".as_ptr() as *const i8),
    );
    let mut locs = LOCS.lock().unwrap_or_else(|p| p.into_inner());
    if locs.len() >= 64 {
        locs.clear();
    }
    locs.push((program, l));
    l
}

/// Draw the FLIR composite where the tonemap quad would have drawn: into the
/// tonemap output target the quad is drawing into, over the whole current
/// viewport, so the FXAA pass that follows presents it. Returns whether the draw
/// happened; a `false` lets the game's own tonemap quad run.
///
/// `ir` = this target's tonemap quad was IR-graded (its own `0x68B8C0` params);
/// `forced` = debug F10 on the main view; `main` = the target was learned as the
/// main view (diagnostics/F10 only); `camera` = the IR-latched camera an IR view
/// takes its config/ambient from (0 for the main view). The composite samples ONLY
/// the G-buffer this target was paired with at its tonemap quad - never "the last
/// G-buffer seen", which belongs to whichever view drew before.
unsafe fn composite_view(
    target: u32,
    ir: bool,
    forced: bool,
    main: bool,
    camera: usize,
    quad: (u32, i32, u32, *const c_void),
) -> bool {
    let (mode, count, kind, indices) = quad;
    // Key of a skipped composite: reason, view size and target.
    let gl = gl();
    let viewport = gl.map_or([0; 4], |gl| current_viewport(gl));
    let skip_key = |reason: u64| -> u64 {
        (reason << 56)
            | ((viewport[2] as u32 as u64) << 32)
            | ((viewport[3] as u32 as u64) << 16)
            | target as u64
    };
    if COMPOSITE_FAILED.load(Ordering::Relaxed) {
        log_composite_skip(forced, skip_key(1), || {
            "the composite program could not be built".to_string()
        });
        return false;
    }
    let Some(gl) = gl else {
        log_composite_skip(forced, skip_key(7), || {
            "the GL entry points are unavailable".to_string()
        });
        return false;
    };
    if viewport[2] <= 0 || viewport[3] <= 0 {
        log_composite_skip(forced, skip_key(2), || {
            format!("the viewport is empty ({}x{})", viewport[2], viewport[3])
        });
        return false;
    }
    #[cfg(feature = "logging")]
    log_view_mode(viewport, target, ir, forced, main);
    // Resize detection is per output target, with the size read at the quad draw.
    if forget_caches_on_resize(target, viewport[2], viewport[3]) {
        // The caches were dropped under this view's feet: keep the game's image
        // for this frame, the next one re-pairs and composites.
        log_composite_skip(forced, skip_key(3), || {
            format!(
                "the tonemap target {target} changed size to {}x{} (caches dropped, retrying next \
frame)",
                viewport[2], viewport[3]
            )
        });
        return false;
    }
    // Only the G-buffer this target was paired with (learned at this or a
    // previous tonemap quad) may be sampled.
    let Some(gbuffer) = paired_gbuffer(target) else {
        log_composite_skip(forced, skip_key(4), || {
            format!(
                "no usable G-buffer is paired with tonemap target {target} yet (the pairing is \
learned at the tonemap quad; targets seen so far: {:?}); the game's tonemap is kept",
                known_targets()
            )
        });
        return false;
    };
    // The game's tonemap program is still bound: take its quad attribute
    // locations and view-projection so our program draws the same geometry.
    let game_program = tracked(gl, &TRACK_PROGRAM, GL_CURRENT_PROGRAM);
    let (pos_loc, coord_loc, mvp_loc) = if game_program > 0 {
        quad_locs(gl, game_program as u32)
    } else {
        (-1, -1, -1)
    };
    if pos_loc < 0 || coord_loc < 0 || mvp_loc < 0 {
        log_composite_skip(forced, skip_key(8), || {
            format!(
                "the game's tonemap program {game_program} lacks the quad inputs (position \
{pos_loc}, coord0 {coord_loc}, mat_view_proj {mvp_loc}); the game's tonemap is kept"
            )
        });
        return false;
    }
    // The matrix hook keeps each program's last mat_view_proj upload (raw,
    // with its transpose flag); only query when it has not seen one.
    let recorded = PROG_MATS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&(game_program as u32))
        .and_then(|m| m.vp);
    let mut mat_view_proj = [0f32; 16];
    match recorded {
        Some((raw, transpose)) => mat_view_proj = to_colmajor(&raw, transpose),
        None => glq!(gl.get_uniformfv, game_program as u32, mvp_loc, mat_view_proj.as_mut_ptr()),
    }
    let Some(composite) = composite(gl, pos_loc as u32, coord_loc as u32) else {
        log_composite_skip(forced, skip_key(6), || {
            "the composite program is unavailable".to_string()
        });
        return false;
    };

    // Config comes from the camera the IR view's tonemap quad ran with (the
    // newest IR-latched camera); the main view (F10) has no camera. Ambient is
    // that camera's pivot temperature, else the scene median cached on the game
    // thread (see `scene_tick`).
    let config = if camera >= PTR_MIN {
        camera_config(camera)
    } else {
        FlirCfg::default()
    };
    let (sun_dir, sun_valid) = sun_toward();
    let ambient = if camera >= PTR_MIN {
        camera_ambient(camera)
    } else {
        scene_ambient()
    };

    let _guard = CompositeGuard::begin(gl);
    (gl.use_program)(composite.program);
    (gl.uniform_matrix4fv)(composite.mat_view_proj, 1, 0, mat_view_proj.as_ptr());
    (gl.active_texture)(GL_TEXTURE0);
    (gl.bind_texture)(GL_TEXTURE_2D, gbuffer.tex_normal);
    (gl.uniform1i)(composite.tex_normal, 0);
    (gl.active_texture)(GL_TEXTURE0 + 1);
    (gl.bind_texture)(GL_TEXTURE_2D, gbuffer.tex_color);
    (gl.uniform1i)(composite.tex_color, 1);
    (gl.active_texture)(GL_TEXTURE0 + 2);
    (gl.bind_texture)(GL_TEXTURE_2D, gbuffer.depth);
    (gl.uniform1i)(composite.tex_depth, 2);
    (gl.uniform1i)(composite.has_depth, (gbuffer.depth != 0) as i32);
    (gl.uniform1i)(
        composite.normal_encoded,
        gbuffer.normal_encoded as i32,
    );
    (gl.uniform3f)(composite.sun_dir, sun_dir[0], sun_dir[1], sun_dir[2]);
    (gl.uniform1f)(composite.sun_valid, sun_valid);
    (gl.uniform1f)(composite.day_pct, f32::from_bits(DAY_PCT_BITS.load(Ordering::Relaxed)));
    (gl.uniform1f)(composite.clear_sky, f32::from_bits(CLEAR_SKY_BITS.load(Ordering::Relaxed)));
    match *LIGHT_INV.lock().unwrap_or_else(|p| p.into_inner()) {
        Some(inv) => {
            (gl.uniform_matrix4fv)(composite.mat_inv, 1, 0, inv.as_ptr());
            (gl.uniform1i)(composite.has_inv, 1);
        }
        None => (gl.uniform1i)(composite.has_inv, 0),
    }
    // In game at altitude: graphics_offset.y = -2000 with the eye at render y
    // -278, i.e. world = render - graphics_offset, so the sea (world y 0) is
    // render y = graphics_offset.y.
    (gl.uniform1f)(composite.water_y, f32::from_bits(LIGHT_OFFSET_Y_BITS.load(Ordering::Relaxed)));
    let ground = f32::from_bits(GROUND_AMBIENT_BITS.load(Ordering::Relaxed));
    (gl.uniform1f)(composite.ambient_c, if ground.is_finite() { ground } else { ambient });
    (gl.uniform1f)(composite.vehicle_air_c, ambient);
    let water_tex = LIGHT_WATER_TEX.load(Ordering::Relaxed);
    (gl.active_texture)(GL_TEXTURE0 + 3);
    (gl.bind_texture)(GL_TEXTURE_2D, water_tex);
    (gl.uniform1i)(composite.tex_water, 3);
    (gl.uniform1i)(composite.has_water, (water_tex != 0) as i32);
    let (pa, pb) = view_plumes();
    (gl.uniform1i)(composite.plume_n, pa.len() as i32);
    if !pa.is_empty() {
        (gl.uniform4fv)(composite.plume_a, pa.len() as i32, pa.as_ptr() as *const f32);
        (gl.uniform4fv)(composite.plume_b, pb.len() as i32, pb.as_ptr() as *const f32);
        log_once_key(0x6a5_0010, || format!("FLIR plume: first drawn {:?} {:?}", pa[0], pb[0]));
    }
    (gl.uniform1f)(composite.sun_soak_c, FLIR_SUN_SOAK_C);
    (gl.uniform3f)(composite.tint, config.rgb[0], config.rgb[1], config.rgb[2]);
    (gl.uniform1i)(composite.black_hot, config.black_hot as i32);
    (gl.uniform1f)(composite.tmin, config.tmin);
    (gl.uniform1f)(composite.tmax, config.tmax);
    (gl.uniform1f)(composite.enc_min, FLIR_ENC_MIN_C);
    (gl.uniform1f)(composite.enc_max, FLIR_ENC_MAX_C);
    (gl.uniform1f)(composite.sky_c, FLIR_SKY_C);
    (gl.uniform1f)(composite.sky_depth_m, FLIR_SKY_DEPTH_M);
    // The game's own VAO/element buffer stay bound: same quad, same indices.
    (gl.draw_elements)(mode, count, kind, indices);
    log_quad_inputs(gl, main, (pos_loc, coord_loc, mvp_loc), &mat_view_proj, quad);
    #[cfg(feature = "logging")]
    {
        // One-shot proof of what the composite wrote (centre pixel, RGBA).
        // Once per (target, G-buffer) pair: a world load re-pairs the main view.
        static DONE: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());
        let first = {
            let mut done = DONE.lock().unwrap_or_else(|p| p.into_inner());
            let new = done.len() < 32 && !done.contains(&(target, gbuffer.fbo));
            if new {
                done.push((target, gbuffer.fbo));
            }
            new
        };
        if first {
            let mut draw = 0i32;
            let mut read = 0i32;
            glq!(gl.get_integerv, GL_FRAMEBUFFER_BINDING, &mut draw);
            glq!(gl.get_integerv, GL_READ_FRAMEBUFFER_BINDING, &mut read);
            (gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, draw as u32);
            let mut px = [f32::NAN; 4];
            let (cx, cy) = (viewport[0] + viewport[2] / 2, viewport[1] + viewport[3] / 2);
            (gl.read_pixels)(cx, cy, 1, 1, GL_RGBA, GL_FLOAT, px.as_mut_ptr() as *mut c_void);
            (gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, read as u32);
            log_line(&format!(
                    "FLIR: composite readback target {target} (FBO {draw}, {} view) centre \
({cx},{cy}) RGBA={px:?} ambient={ambient:.1} window=[{}, {}]",
                    if main { "main" } else { "feed/other" },
                    config.tmin,
                    config.tmax
            ));
        }
    }
    drop(_guard);

    let pair_key = 0x06a5_4000 | ((gbuffer.fbo as u64 & 0x7f) << 7) | (target as u64 & 0x7f);
    log_once_key(pair_key, || {
        format!(
            "FLIR: composite drawn {}x{} into tonemap target {target} ({} view, IR={} F10={}): \
tint={:?} black_hot={} window=[{}, {}] C ambient={:.1} C gbuffer=(fbo {}, {}x{}, normal {}, \
colour {}, depth {})",
            viewport[2],
            viewport[3],
            if main { "main" } else { "feed/other" },
            ir as u8,
            forced as u8,
            config.rgb,
            config.black_hot,
            config.tmin,
            config.tmax,
            ambient,
            gbuffer.fbo,
            gbuffer.size[0],
            gbuffer.size[1],
            gbuffer.tex_normal,
            gbuffer.tex_color,
            gbuffer.depth
        )
    });
    true
}

fn async_key_down(vk: i32) -> Option<bool> {
    static GET_KEY_STATE: OnceLock<usize> = OnceLock::new();
    let address = *GET_KEY_STATE.get_or_init(|| unsafe {
        let module = LoadLibraryA(b"user32.dll\0".as_ptr());
        if module.is_null() {
            return 0;
        }
        GetProcAddress(module, b"GetAsyncKeyState\0".as_ptr()).map_or(0, |f| f as usize)
    });
    if address == 0 {
        return None;
    }
    let f: unsafe extern "system" fn(i32) -> i16 = unsafe { std::mem::transmute(address) };
    Some((unsafe { f(vk) } as u16) & 0x8000 != 0)
}

/// One-shot (per main / feed view) log of the game's quad inputs: attribute
/// locations, `mat_view_proj` and the min/max of the texcoords the quad feeds.
unsafe fn log_quad_inputs(
    gl: &'static Gl,
    main: bool,
    locs: (i32, i32, i32),
    mvp: &[f32; 16],
    quad: (u32, i32, u32, *const c_void),
) {
    static DONE_MAIN: AtomicBool = AtomicBool::new(false);
    static DONE_FEED: AtomicBool = AtomicBool::new(false);
    if (if main { &DONE_MAIN } else { &DONE_FEED }).swap(true, Ordering::Relaxed) {
        return;
    }
    const ARRAY_BUFFER: u32 = 0x8892;
    const ELEMENT_ARRAY_BUFFER: u32 = 0x8893;
    const ARRAY_BUFFER_BINDING: u32 = 0x8894;
    let (mode, count, kind, indices) = quad;
    let (pos_loc, coord_loc, _) = locs;
    let coords = (|| {
        let (mut vbo, mut size, mut ty, mut stride) = (0i32, 0i32, 0i32, 0i32);
        glq!(gl.get_vertex_attribiv, coord_loc as u32, 0x889f, &mut vbo);
        glq!(gl.get_vertex_attribiv, coord_loc as u32, 0x8623, &mut size);
        glq!(gl.get_vertex_attribiv, coord_loc as u32, 0x8625, &mut ty);
        glq!(gl.get_vertex_attribiv, coord_loc as u32, 0x8624, &mut stride);
        let mut ptr: *const c_void = std::ptr::null();
        glq!(gl.get_vertex_attrib_pointerv, coord_loc as u32, 0x8645, &mut ptr);
        let (mut ibo, mut ebo_size) = (0i32, 0i32);
        glq!(gl.get_integerv, GL_ELEMENT_ARRAY_BUFFER_BINDING, &mut ibo);
        let width = match kind {
            0x1401 => 1usize,
            0x1403 => 2,
            0x1405 => 4,
            _ => return None,
        };
        if vbo <= 0 || ty as u32 != GL_FLOAT || size < 2 || ibo <= 0 || !(1..=64).contains(&count)
        {
            return None;
        }
        let n = count as usize;
        glq!(gl.get_buffer_parameteriv, ELEMENT_ARRAY_BUFFER, 0x8764, &mut ebo_size);
        let first = indices as usize;
        if first + n * width > ebo_size.max(0) as usize {
            return None;
        }
        let mut raw = vec![0u8; n * width];
        glq!(gl.get_buffer_sub_data, ELEMENT_ARRAY_BUFFER, first as isize, (n * width) as isize, raw.as_mut_ptr() as *mut c_void);
        let idx: Vec<usize> = raw
            .chunks(width)
            .map(|c| c.iter().rev().fold(0usize, |a, &b| (a << 8) | b as usize))
            .collect();
        let step = if stride > 0 { stride as usize } else { size as usize * 4 };
        let (mut prev, mut bsize) = (0i32, 0i32);
        glq!(gl.get_integerv, ARRAY_BUFFER_BINDING, &mut prev);
        (gl.bind_buffer)(ARRAY_BUFFER, vbo as u32);
        glq!(gl.get_buffer_parameteriv, ARRAY_BUFFER, 0x8764, &mut bsize);
        let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
        let mut any = false;
        for i in idx {
            let o = ptr as usize + i * step;
            if o + 8 > bsize.max(0) as usize {
                continue;
            }
            let mut v = [0f32; 2];
            glq!(gl.get_buffer_sub_data, ARRAY_BUFFER, o as isize, 8, v.as_mut_ptr() as *mut c_void);
            for a in 0..2 {
                lo[a] = lo[a].min(v[a]);
                hi[a] = hi[a].max(v[a]);
            }
            any = true;
        }
        (gl.bind_buffer)(ARRAY_BUFFER, prev as u32);
        any.then_some((lo, hi))
    })();
    log_line(&format!(
        "FLIR: game quad ({} view): draw mode={mode:#x} count={count} type={kind:#x}; attrib \
vertex_position_in={pos_loc} vertex_coord0_in={coord_loc} mat_view_proj loc={} = {mvp:?}; coord0 \
min/max={}",
        if main { "main" } else { "feed/other" },
        locs.2,
        match coords {
            Some((lo, hi)) => format!("{lo:?}/{hi:?}"),
            None => "unreadable".to_string(),
        }
    ));
}

/// One line per distinct reason a composite was not drawn, so a missing FLIR
/// image always names its cause. F10-forced attempts are labelled separately:
/// they have no IR grading signature to point at.
fn log_composite_skip(forced: bool, key: u64, message: impl FnOnce() -> String) {
    // Bit 63 separates the F10-forced reasons from the flag-driven ones.
    // Exempt 0x6a5 range (a missing composite must always name its cause):
    // reason (top byte of `key`), forced flag, low bits of the target.
    let key = 0x06a5_1000 | ((key >> 56) & 0xf) << 8 | (forced as u64) << 7 | (key & 0x7f);
    log_once_key(key, || {
        let prefix = if forced {
            "FLIR: F10 main-view composite skipped: "
        } else {
            "FLIR: composite skipped: "
        };
        format!("{prefix}{}", message())
    });
}

fn camera_config(camera: usize) -> FlirCfg {
    FLIR_CONFIG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&camera)
        .copied()
        .unwrap_or_default()
}

fn sun_toward() -> ([f32; 3], f32) {
    let calc = *GAME_SUN.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(l) = *LIGHT_SUN.lock().unwrap_or_else(|p| p.into_inner()) {
        let v = [-l[0], -l[1], -l[2]];
        #[cfg(feature = "logging")]
        {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let now = now_ms();
            if now >= NEXT.load(Ordering::Relaxed) {
                NEXT.store(now + 5000, Ordering::Relaxed);
                let el = |d: [f32; 3]| d[1].clamp(-1.0, 1.0).asin().to_degrees();
                let az = |d: [f32; 3]| d[2].atan2(d[0]).to_degrees();
                let diff = calc.map(|c| (c[0] * v[0] + c[1] * v[1] + c[2] * v[2]).clamp(-1.0, 1.0).acos().to_degrees());
                // Eye = inverse view-projection of the near-plane centre.
                let eye_y = LIGHT_INV.lock().unwrap_or_else(|p| p.into_inner()).map(|m| {
                    let w = -m[11] + m[15];
                    (-m[9] + m[13]) / w
                });
                let offset_y = f32::from_bits(LIGHT_OFFSET_Y_BITS.load(Ordering::Relaxed));
                log_line(&format!(
                    "FLIR sun: light-pass to_sun ({:+.4},{:+.4},{:+.4}) el {:+.2} az {:+.2} | calc {:?} el {:?} az {:?} | angle {:?} deg | eye_y(render) {:?} graphics_offset.y {:.1}",
                    v[0], v[1], v[2], el(v), az(v), calc, calc.map(el), calc.map(az), diff, eye_y, offset_y
                ));
            }
        }
        return (v, if v[1] > 0.0 { 1.0 } else { 0.0 });
    }
    if let Some(v) = calc {
        return (v, if v[1] > 0.0 { 1.0 } else { 0.0 });
    }
    if !LIGHT_DIR_VALID.load(Ordering::Relaxed) {
        return ([0.0, 1.0, 0.0], 0.0);
    }
    let dir = *LIGHT_DIR.lock().unwrap_or_else(|p| p.into_inner());
    // The game's sun_light_direction points from the sun into the scene; the
    // shader soaks surfaces with -dot(dir, normal), so flip it here.
    let v = [-dir[0] as f32, -dir[1] as f32, -dir[2] as f32];
    let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if n < 1.0e-4 {
        return ([0.0, 1.0, 0.0], 0.0);
    }
    let valid = if v[1] > 0.0 { 1.0 } else { 0.0 };
    ([v[0] / n, v[1] / n, v[2] / n], valid)
}

/// Build (once) the composite program. A failed build disables the composite for
/// the session (fail closed) instead of retrying every frame.
fn composite(gl: &'static Gl, pos_loc: u32, coord_loc: u32) -> Option<Composite> {
    let mut slot = COMPOSITE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(composite) = slot.as_ref() {
        if composite.pos_loc == pos_loc && composite.coord_loc == coord_loc {
            return Some(*composite);
        }
        // The game's quad inputs moved: relink with the new locations.
        unsafe { (gl.delete_program)(composite.program) };
        *slot = None;
    }
    if COMPOSITE_FAILED.load(Ordering::Relaxed) {
        return None;
    }
    unsafe {
        let Some(program) = link_program(gl, COMPOSITE_VS, COMPOSITE_FS, |gl, program| {
            (gl.bind_attrib_location)(program, pos_loc, b"vertex_position_in\0".as_ptr() as *const i8);
            (gl.bind_attrib_location)(program, coord_loc, b"vertex_coord0_in\0".as_ptr() as *const i8);
        }) else {
            COMPOSITE_FAILED.store(true, Ordering::Relaxed);
            return None;
        };
        let entry = Composite {
            program,
            tex_normal: uniform(gl, program, b"tex_normal\0"),
            tex_color: uniform(gl, program, b"tex_color\0"),
            tex_depth: uniform(gl, program, b"tex_depth\0"),
            has_depth: uniform(gl, program, b"has_depth\0"),
            mat_view_proj: uniform(gl, program, b"mat_view_proj\0"),
            normal_encoded: uniform(gl, program, b"normal_encoded\0"),
            sun_dir: uniform(gl, program, b"sun_dir\0"),
            sun_valid: uniform(gl, program, b"sun_valid\0"),
            day_pct: uniform(gl, program, b"day_pct\0"),
            clear_sky: uniform(gl, program, b"clear_sky\0"),
            mat_inv: uniform(gl, program, b"mat_inv\0"),
            vehicle_air_c: uniform(gl, program, b"vehicle_air_c\0"),
            tex_water: uniform(gl, program, b"tex_water\0"),
            has_water: uniform(gl, program, b"has_water\0"),
            plume_a: uniform(gl, program, b"plume_a\0"),
            plume_b: uniform(gl, program, b"plume_b\0"),
            plume_n: uniform(gl, program, b"plume_n\0"),
            has_inv: uniform(gl, program, b"has_inv\0"),
            water_y: uniform(gl, program, b"water_y\0"),
            ambient_c: uniform(gl, program, b"ambient_c\0"),
            sun_soak_c: uniform(gl, program, b"sun_soak_c\0"),
            tint: uniform(gl, program, b"tint\0"),
            black_hot: uniform(gl, program, b"black_hot\0"),
            tmin: uniform(gl, program, b"tmin\0"),
            tmax: uniform(gl, program, b"tmax\0"),
            enc_min: uniform(gl, program, b"enc_min\0"),
            enc_max: uniform(gl, program, b"enc_max\0"),
            sky_c: uniform(gl, program, b"sky_c\0"),
            sky_depth_m: uniform(gl, program, b"sky_depth_m\0"),
            pos_loc,
            coord_loc,
        };
        *slot = Some(entry);
        Some(entry)
    }
}

// ---------------------------------------------------------------------------
// Shader helpers
// ---------------------------------------------------------------------------

unsafe fn uniform(gl: &'static Gl, program: u32, name: &[u8]) -> i32 {
    glq!(gl.get_uniform_location, program, name.as_ptr() as *const i8)
}

unsafe fn link_program(
    gl: &'static Gl,
    vertex_source: &str,
    fragment_source: &str,
    before_link: impl FnOnce(&'static Gl, u32),
) -> Option<u32> {
    let vertex = compile_shader(gl, GL_VERTEX_SHADER, vertex_source, "vertex")?;
    let fragment = match compile_shader(gl, GL_FRAGMENT_SHADER, fragment_source, "fragment") {
        Some(shader) => shader,
        None => {
            (gl.delete_shader)(vertex);
            return None;
        }
    };
    let program = (gl.create_program)();
    if program == 0 {
        (gl.delete_shader)(vertex);
        (gl.delete_shader)(fragment);
        log_once(LOG_FLIR_SHADER, || {
            "FLIR: glCreateProgram failed; FLIR shader pass disabled".to_string()
        });
        return None;
    }
    (gl.attach_shader)(program, vertex);
    (gl.attach_shader)(program, fragment);
    before_link(gl, program);
    (gl.link_program)(program);
    let mut status = 0i32;
    glq!(gl.get_programiv, program, GL_LINK_STATUS, &mut status);
    (gl.delete_shader)(vertex);
    (gl.delete_shader)(fragment);
    if status == 0 {
        let log = program_log(gl, program);
        log_once(LOG_FLIR_SHADER, || {
            format!("FLIR: program link failed; FLIR shader pass disabled: {log}")
        });
        (gl.delete_program)(program);
        return None;
    }
    Some(program)
}

/// Trim a GL info log to what one log line can hold.
fn info_log_text(buffer: &[u8]) -> String {
    String::from_utf8_lossy(buffer)
        .trim_end_matches('\0')
        .chars()
        .take(800)
        .collect()
}

unsafe fn program_log(gl: &'static Gl, program: u32) -> String {
    let mut length = 0i32;
    glq!(gl.get_programiv, program, GL_INFO_LOG_LENGTH, &mut length);
    let mut buffer = vec![0u8; length.max(1) as usize];
    (gl.get_program_info_log)(
        program,
        length.max(1),
        std::ptr::null_mut(),
        buffer.as_mut_ptr() as *mut i8,
    );
    info_log_text(&buffer)
}

/// Same for a shader object: only the fetch differs (the log query is
/// object-kind specific), the trimming is shared with [`program_log`].
unsafe fn shader_log(gl: &'static Gl, shader: u32) -> String {
    let mut length = 0i32;
    (gl.get_shaderiv)(shader, GL_INFO_LOG_LENGTH, &mut length);
    let mut buffer = vec![0u8; length.max(1) as usize];
    (gl.get_shader_info_log)(
        shader,
        length.max(1),
        std::ptr::null_mut(),
        buffer.as_mut_ptr() as *mut i8,
    );
    info_log_text(&buffer)
}

unsafe fn compile_shader(gl: &'static Gl, kind: u32, source: &str, label: &str) -> Option<u32> {
    let shader = (gl.create_shader)(kind);
    if shader == 0 {
        log_once(LOG_FLIR_SHADER, || {
            format!("FLIR: glCreateShader({label}) failed; FLIR shader pass disabled")
        });
        return None;
    }
    let bytes = source.as_bytes();
    let length = bytes.len() as i32;
    let pointer = bytes.as_ptr() as *const i8;
    (gl.shader_source)(shader, 1, &pointer, &length);
    (gl.compile_shader)(shader);
    let mut status = 0i32;
    (gl.get_shaderiv)(shader, GL_COMPILE_STATUS, &mut status);
    if status == 0 {
        let log = shader_log(gl, shader);
        log_once(LOG_FLIR_SHADER, || {
            format!("FLIR: {label} shader compile failed; FLIR shader pass disabled: {log}")
        });
        (gl.delete_shader)(shader);
        return None;
    }
    Some(shader)
}
