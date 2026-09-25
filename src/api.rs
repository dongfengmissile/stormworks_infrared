//! The Lua surface the components call: `ir.probe` and `ir.scan`, plus the
//! table registration the component registrar hook invokes.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::heat::{base_ambient_at, temperature_at, variant_args};
#[cfg(feature = "logging")]
use crate::heat::{log_fires_container, log_heat_container, log_volume_heat_sources};
use crate::ir::{
    axes_for, cross, normalize, project_source, raycast_hit, read_component_matrix, IrSource,
};
#[cfg(feature = "logging")]
use crate::ir::{weather_fog_rain, weather_transmittance};
use crate::logging::log_line;
use crate::lua::*;
use crate::memory::{game_fn, read_usize_at};
#[cfg(feature = "logging")]
use crate::memory::{read_f32_at, read_u32_at};
use crate::offsets::*;
use crate::sources::{
    collect_emitter_sources, collect_fire_sources, collect_ir_sources, collect_volume_heat_sources,
};
use crate::state::*;
use crate::sun::add_sun_source;
#[cfg(feature = "logging")]
use crate::sun::scene_env;

/// Register the `ir` global into the component-mod Lua state, exactly the way
/// the game registers its own `matrix` / `parser` / `component` tables.
pub(crate) unsafe fn register_ir_table(context: usize) {
    if context == 0 {
        return;
    }
    let alloc: unsafe extern "C" fn(*mut usize, u32) = game_fn(RVA_STRING_ALLOC);
    let fill: unsafe extern "C" fn(usize, *const u8, u32) = game_fn(RVA_STRING_FILL);
    let register: unsafe extern "C" fn(usize, *const usize, *const FnPair, usize) =
        game_fn(RVA_REGISTER_TABLE);

    let mut name_object: usize = 0;
    alloc(&mut name_object, 3);
    if name_object == 0 {
        log_line("ir registration: name allocation failed");
        return;
    }
    fill(name_object, b"ir\0".as_ptr(), 3);

    let pairs = [
        FnPair {
            name: b"scan\0".as_ptr(),
            func: ir_scan as *const () as usize,
        },
        FnPair {
            name: b"probe\0".as_ptr(),
            func: ir_probe as *const () as usize,
        },
        FnPair {
            name: std::ptr::null(),
            func: 0,
        },
    ];

    register(context, &name_object as *const usize, pairs.as_ptr(), context + 0x28);
}

/// Register the `ir` table into the drawing (screen) Lua environment.
///
/// The same three calls as the component-mod registration. That matters: the
/// game's registrar creates a fresh table and sets it under the name, so if
/// this state turned out to be the same one the component registrar already
/// served, a shorter table would silently replace `ir` and take `ir.scan` away
/// from the seeker. An identical table makes that replacement a no-op.
///
/// `ir.scan` from a state with no component context is harmless rather than
/// dangerous: lua_component_handle only accepts the two upvalue tags the game
/// uses, so it yields 0 and the scan simply finds nothing.
pub(crate) unsafe fn register_ir_screen_table(state: usize) {
    if state == 0 {
        return;
    }
    let alloc: unsafe extern "C" fn(*mut usize, u32) = game_fn(RVA_STRING_ALLOC);
    let fill: unsafe extern "C" fn(usize, *const u8, u32) = game_fn(RVA_STRING_FILL);
    let register: unsafe extern "C" fn(usize, *const usize, *const FnPair, usize) =
        game_fn(RVA_REGISTER_TABLE);

    let mut name_object: usize = 0;
    alloc(&mut name_object, 3);
    if name_object == 0 {
        log_line("ir screen registration: name allocation failed");
        return;
    }
    fill(name_object, b"ir\0".as_ptr(), 3);

    let pairs = [
        FnPair {
            name: b"scan\0".as_ptr(),
            func: ir_scan as *const () as usize,
        },
        FnPair {
            name: b"frame\0".as_ptr(),
            func: ir_frame as *const () as usize,
        },
        FnPair {
            name: b"probe\0".as_ptr(),
            func: ir_probe as *const () as usize,
        },
        FnPair {
            name: std::ptr::null(),
            func: 0,
        },
    ];

    // The game's own initializers hand register_table a stack slot to receive
    // the table, so a local is the safe destination here.
    let mut destination: usize = 0;
    register(
        state,
        &name_object as *const usize,
        pairs.as_ptr(),
        &mut destination as *mut usize as usize,
    );
}

// ---------------------------------------------------------------------------
// injected Lua functions
// ---------------------------------------------------------------------------

pub(crate) extern "C" fn ir_probe(lua: *mut c_void) -> i32 {
    let lua = lua as usize;
    unsafe {
        let x = lua_arg_number(lua, 1).unwrap_or(0.0);
        let y = lua_arg_number(lua, 2).unwrap_or(0.0);
        let z = lua_arg_number(lua, 3).unwrap_or(0.0);
        let temperature = temperature_at(x, y, z) as f64;
        lua_push_number(lua, temperature);
    }
    1
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
pub(crate) extern "C" fn ir_frame(lua: *mut c_void) -> i32 {
    let lua = lua as usize;
    unsafe {
        // arg 1: which seeker's frame. 0 (or omitted) returns nothing, because
        // a seeker's id is never 0 - the drawing script passes what it read from
        // the composite, so it asks for its own seeker and no other.
        let id = (lua_arg_number(lua, 1).unwrap_or(0.0) as i64)
            .clamp(0, FRAME_ID_MAX as i64) as u32;
        let (width, height, bytes) = {
            let frames = LAST_FRAME
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match frames.get(&id) {
                Some((width, height, bytes)) => (*width, *height, bytes.clone()),
                None => (0, 0, Vec::new()),
            }
        };
        lua_push_table(lua);
        for (index, value) in bytes.iter().enumerate() {
            lua_push_number(lua, *value as f64);
            lua_rawseti(lua, index as i64 + 1);
        }
        lua_push_number(lua, width as f64);
        lua_push_number(lua, height as f64);
    }
    3
}

/// A seeker's identity for the frame map: 24 bits derived from its component.
///
/// Live components have distinct pointers, so this is unique among the seekers
/// actually running, without anyone having to number them - which is the point:
/// a channel number is global to the whole game process, so two vehicles that
/// both picked 1 would overwrite each other's image. It changes when a vehicle
/// is respawned, which the drawing side simply re-reads.
pub(crate) fn seeker_id(component: usize) -> u32 {
    if component == 0 {
        return 0;
    }
    // Mix the whole pointer down to 24 bits rather than truncating it, so two
    // components that differ only in their high bits still separate, and the
    // value does not read as a raw address. Deterministic for a given component.
    let mut mixed = (component as u64) ^ 0x9e37_79b9_7f4a_7c15;
    mixed = mixed.wrapping_mul(0xff51_afd7_ed55_8ccd);
    mixed ^= mixed >> 29;
    let id = (mixed & FRAME_ID_MAX as u64) as u32;
    if id == 0 {
        1
    } else {
        id
    }
}

// One heat source as seen by the IR sensor.
pub(crate) extern "C" fn ir_scan(lua: *mut c_void) -> i32 {
    let lua = lua as usize;
    unsafe {
        let az_steer = lua_arg_number(lua, 1).unwrap_or(0.0);
        let el_steer = lua_arg_number(lua, 2).unwrap_or(0.0);
        let fov = lua_arg_number(lua, 3).unwrap_or(0.0833);
        let range = lua_arg_number(lua, 4).unwrap_or(200.0);
        let res_x = (lua_arg_number(lua, 5).unwrap_or(8.0) as i32).clamp(1, 64);
        let res_y = (lua_arg_number(lua, 6).unwrap_or(4.0) as i32).clamp(1, 64);
        // Arg 7 = IR gain: flux that maps to full-scale white.
        // Arg 8 = detect threshold (raw flux); the Lua applies it.
        let gain = lua_arg_number(lua, 7).unwrap_or(0.1);
        let _detect_flux = lua_arg_number(lua, 8).unwrap_or(0.01);
        let axis = lua_arg_number(lua, 9).unwrap_or(4.0) as i32;
        let occlude = lua_arg_number(lua, 10).unwrap_or(1.0) != 0.0;
        // arg 11: pack the image 2 cells per float (12-bit each) when the grid
        // is larger than one composite channel set (33..64 cells).
        let pack = lua_arg_number(lua, 11).unwrap_or(0.0) != 0.0;
        // arg 12: whether to return the cell table. A seeker whose image reaches
        // the microcontroller through ir.frame (rather than a composite) can skip
        // it: at 64x64 that is 4096 Lua table insertions per scan for values
        // nobody reads. An empty table is still returned, to keep the order of
        // the other return values.
        let cells_out = lua_arg_number(lua, 12).unwrap_or(1.0) != 0.0;


        let cell_count = (res_x * res_y) as usize;
        let mut cells = vec![0.0f64; cell_count];
        let mut az_error = 0.0f64;
        let mut el_error = 0.0f64;
        let mut strength = 0.0f64;
        let mut total_flux = 0.0f64;
        let mut base_temp = 20.0f64;
        let mut source_count = 0usize;

        // Diagnostics-only scan state for the throttled verbose log.
        #[cfg(feature = "logging")]
        let mut diag_fwd = (0.0f64, 0.0f64, 0.0f64);
        #[cfg(feature = "logging")]
        let mut diag_visible: Vec<(f64, f64, f64, f64, f64)> = Vec::new();
        #[cfg(feature = "logging")]
        let mut diag_fov = 0.0f64;
        #[cfg(feature = "logging")]
        let mut diag_cone = 0.0f64;
        #[cfg(feature = "logging")]
        let mut diag_steer = (0.0f64, 0.0f64);
        // Normalised centroid (wx/ws, wy/ws, weight_sum) for the throttled
        // dump - lets a pinned-at-centre error be told apart from a real one.
        #[allow(unused_assignments)]
        let mut diag_centroid = (0.0f64, 0.0f64, 0.0f64);
        let mut packed_vals: Vec<f64> = Vec::new();

        let component = lua_component(lua);
        let seeker_id = seeker_id(component);
        #[allow(unused_assignments)]
        let mut origin = (0.0f64, 0.0f64, 0.0f64);
        // The scan runs whenever the component exists; without a temperature
        // evaluation yet (no engine/probe this session) heat sources and the
        // real ambient are unavailable, but fires/flares still are (fires
        // holder cached by the fire-render hook).
        // A foreign Lua state can hand back a pointer that is not a component at
        // all, so the vtable is checked before anything is read through it.
        if component != 0 && crate::hooks::looks_like_component(component) {
            let matrix = read_component_matrix(component);
            origin = (matrix[12], matrix[13], matrix[14]);
            let (forward, up) = axes_for(axis, &matrix);
            let forward = normalize(forward);
            // right = up x fwd. With the game's handedness this is the seeker's
            // physical right for the +Y-forward mesh mount (verified in game:
            // the other order mirrors the array image left-right).
            let right = normalize(cross(up, forward));

            let fov_rad = fov * TAU;
            let az_center = az_steer * TAU;
            let el_center = el_steer * TAU;

            // One line per plugin load: proves which cone geometry is running
            // and records the configured FOV, so "is the game executing the
            // new build" is answerable from the player build's bootstrap log.
            static CONE_LOGGED: AtomicBool = AtomicBool::new(false);
            if !CONE_LOGGED.swap(true, Ordering::Relaxed) {
                log_line(&format!(
                    "scan armed: fov={fov:.4} turns ({:.2} deg full, {:.2} deg half-angle), \nerror sources: in-cone centroid only",
                    fov_rad.to_degrees(),
                    (fov_rad * 0.5).to_degrees()
                ));
            }

            let mut weight_sum = 0.0f64;
            let mut weight_x = 0.0f64;
            let mut weight_y = 0.0f64;
            let az_sp = fov_rad / res_x as f64;
            let el_sp = fov_rad / res_y as f64;
            let cell_solid = az_sp * el_sp;

            // Collect every heat source and its emitted IR. This is radiometry
            // (emission ~ T^4 * area, received ~ 1/d^2), not the game's local
            // temperature field, so distant hot sources stay visible.
            let mut sources: Vec<IrSource> = Vec::new();
            let vcount = VARIANT_COUNT.load(Ordering::Relaxed).min(MAX_VARIANTS);
            let mut fires_holders = [0usize; MAX_VARIANTS + 1];
            let mut fires_n = 0usize;
            for i in 0..vcount {
                let a = variant_args(i);
                collect_ir_sources(a[0], &origin, range, &mut sources);
                collect_volume_heat_sources(a[0], &origin, range, &mut sources);
                let h = a[3];
                if h >= 0x10000 && !fires_holders[..fires_n].contains(&h) {
                    fires_holders[fires_n] = h;
                    fires_n += 1;
                }
            }
            let fh = FIRES_HOLDER.load(Ordering::Relaxed);
            if fh >= 0x10000 && !fires_holders[..fires_n].contains(&fh) {
                fires_holders[fires_n] = fh;
                fires_n += 1;
            }
            for &h in &fires_holders[..fires_n] {
                collect_fire_sources(h, &origin, range, &mut sources);
            }
            collect_emitter_sources(&origin, range, &mut sources);
            add_sun_source(origin, &mut sources);
            source_count = sources.len();
            base_temp = base_ambient_at(origin) as f64;
            for s in sources.iter_mut() {
                project_source(s, origin, forward, right, up);
            }

            // Line-of-sight: one physics ray per source. Blockers close to the
            // sensor (its own airframe) or to the emitter (its own hull edge)
            // are forgiven so enclosed emitters still glow.
            if occlude {
                let handle = lua_component_handle(lua);
                let ctx = if handle != 0 {
                    read_usize_at(handle + 8)
                } else {
                    0
                };
                let physics_ok = component != 0 && read_usize_at(component + 0x1d8) != 0;
                if physics_ok {
                    sources.retain(|s| {
                        let center_dist =
                            (origin.0 - s.x) * (origin.0 - s.x)
                                + (origin.1 - s.y) * (origin.1 - s.y)
                                + (origin.2 - s.z) * (origin.2 - s.z);
                        let center_dist = center_dist.sqrt();
                        match raycast_hit(ctx, origin, (s.x, s.y, s.z)) {
                            Some(hit_dist) => {
                                !(hit_dist > OCCLUDE_MIN_DIST
                                    && hit_dist
                                        < center_dist - s.r_eff - OCCLUDE_END_SLACK)
                            }
                            None => true,
                        }
                    });
                    source_count = sources.len();
                }
            }

            // Pre-cone snapshot for verbose diagnostics only.
            #[cfg(feature = "logging")]
            {
                diag_fwd = forward;
                diag_fov = fov_rad;
                diag_cone = fov_rad * 0.5;
                diag_steer = (az_center, el_center);
                diag_visible = sources
                    .iter()
                    .map(|s| {
                        (
                            s.az,
                            s.el,
                            s.flux,
                            s.theta,
                            if s.sky { 1.0 } else { 0.0 },
                        )
                    })
                    .collect();
            }

            // Guidance comes from source directions, not the raster centroid:
            // a sub-pixel emitter can straddle cells symmetrically and make
            // that centroid exactly boresight while the source is off-axis.
            // Prefer the brightest in-FOV emitter; with an empty FOV, use the
            // brightest forward-hemisphere emitter so a gimbal can slew to it.
            // Sky sources remain visible, but do not steer the seeker.
            let mut brightest_in_cone: Option<(f64, f64, f64)> = None;
            let mut brightest_forward: Option<(f64, f64, f64)> = None;
            for s in &sources {
                if s.sky || !s.flux.is_finite() || s.flux <= 0.0 {
                    continue;
                }
                let d = s.az - az_center;
                let daz = d - TAU * (d / TAU).round();
                let del = s.el - el_center;
                let off = ((daz * s.el.cos().max(0.2)).powi(2) + del * del).sqrt();
                let candidate = (s.flux, daz, del);
                let forward_dot = s.el.sin() * el_center.sin()
                    + s.el.cos() * el_center.cos() * daz.cos();
                if forward_dot > 0.0
                    && brightest_forward.map_or(true, |best| s.flux > best.0)
                {
                    brightest_forward = Some(candidate);
                }
                if off <= fov_rad * 0.5
                    && brightest_in_cone.map_or(true, |best| s.flux > best.0)
                {
                    brightest_in_cone = Some(candidate);
                }
            }
            if let Some((_, daz, del)) = brightest_in_cone.or(brightest_forward) {
                // Absolute angular offsets in turns, independent of FOV and
                // pixel resolution (0.37 degrees is about 0.00103 turns).
                az_error = daz / TAU;
                el_error = del / TAU;
            }

            // The configured FOV still gates the rendered image, detection
            // flux and source count; steering above is independent of raster.
            sources.retain(|s| {
                let d = s.az - az_center;
                let daz = d - TAU * (d / TAU).round();
                let del = s.el - el_center;
                let a = daz * s.el.cos().max(0.2);
                let inside = (a * a + del * del).sqrt() <= fov_rad * 0.5;
                if inside {
                    total_flux += s.flux;
                }
                inside
            });
            source_count = sources.len();

            // Snapshot the survivors for the heartbeat: distance, effective
            // radius and flux are enough to identify an emitter and back-solve
            // its temperature, which is exactly what a faint reading needs.
            if let Ok(mut snapshot) = LAST_SOURCES.lock() {
                snapshot.clear();
                for s in sources.iter().take(8) {
                    let dx = s.x - origin.0;
                    let dy = s.y - origin.1;
                    let dz = s.z - origin.2;
                    snapshot.push((
                        (dx * dx + dy * dy + dz * dz).sqrt(),
                        s.r_eff,
                        s.flux,
                        s.sky,
                        s.temp_c,
                    ));
                }
            }

            // Rasterize. Unresolved sources (angular radius under one cell)
            // land on the nearest cell with a one-cell spill, so a sub-cell
            // target can never slip between cell centres. Resolved sources
            // dilute per cell (each pixel sees only part of them), which keeps
            // per-pixel brightness radiance-conserving.
            for s in &sources {
                let d = s.az - az_center;
                let daz = d - TAU * (d / TAU).round();
                let del = s.el - el_center;
                let col_f = (daz / fov_rad + 0.5) * res_x as f64 - 0.5;
                let row_f = (0.5 - del / fov_rad) * res_y as f64 - 0.5;
                let solid = std::f64::consts::PI * s.theta * s.theta;
                let dilut = if solid <= cell_solid {
                    1.0
                } else {
                    cell_solid / solid
                };
                // How far past full scale this source's core would clip, which
                // is the light available to bloom into the surrounding cells.
                let excess = if gain > 1e-9 {
                    (s.flux * dilut / gain - 1.0).max(0.0)
                } else {
                    0.0
                };
                let glare = BLOOM_PEAK * (1.0 - (-excess).exp());
                for row in 0..res_y {
                    for col in 0..res_x {
                        let dc = col_f - col as f64;
                        let dr = row_f - row as f64;
                        let d_cells = (dc * dc + dr * dr).sqrt();
                        let w = if solid <= cell_solid {
                            (1.2 - d_cells).clamp(0.0, 1.0)
                        } else {
                            let a = dc * az_sp * s.el.cos().max(0.2);
                            let b = dr * el_sp;
                            let ang = (a * a + b * b).sqrt();
                            ((s.theta - ang) / (0.3 * s.theta)).clamp(0.0, 1.0)
                        };
                        let idx = (row * res_x + col) as usize;
                        let halo = glare
                            * (-(d_cells / BLOOM_RADIUS_CELLS).powi(2)).exp()
                            * gain;
                        let nv = (s.flux * dilut * w).max(halo);
                        if nv > cells[idx] {
                            cells[idx] = nv;
                        }
                    }
                }
            }

            // Keep the raw image centroid for diagnostics only. Guidance is
            // computed from source angles above, before FOV rasterization, so a
            // sub-pixel emitter cannot disappear into a symmetric pixel blob.
            for (index, value) in cells.iter().enumerate() {
                let col = (index as i32 % res_x) as f64;
                let row = (index as i32 / res_x) as f64;
                weight_sum += value;
                weight_x += value * (col + 0.5) / res_x as f64;
                weight_y += value * (0.5 - (row + 0.5) / res_y as f64);
            }
            diag_centroid = (
                weight_x / weight_sum.max(1e-9),
                weight_y / weight_sum.max(1e-9),
                weight_sum,
            );

            // Normalize the display image independently of guidance.
            for value in cells.iter_mut() {
                let norm = if gain > 1e-9 { *value / gain } else { 0.0 };
                *value = norm.clamp(0.0, 1.0);
            }
            strength = if gain > 1e-9 {
                (total_flux / gain).clamp(0.0, 1.0)
            } else {
                0.0
            };

            // Publish the rendered frame for the drawing environment, under this
            // seeker's identity. A logic composite is 32 floats - 64 packed
            // cells - so a 64x64 image can only leave the component through the
            // Lua state; ir.frame(id) reads this snapshot from the
            // microcontroller side.
            {
                let bytes: Vec<u8> = cells
                    .iter()
                    .map(|value| (value.clamp(0.0, 1.0) * 255.0) as u8)
                    .collect();
                let mut frames = LAST_FRAME
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if frames.len() >= FRAME_MAP_MAX && !frames.contains_key(&seeker_id) {
                    frames.clear();
                }
                frames.insert(seeker_id, (res_x, res_y, bytes));
            }

            // Optional packing for grids above one composite channel set
            // (33..64 cells): 2 cells per float, 12 bits each — value =
            // high*4096 + low. Exact in an f32 composite channel; the receiving
            // micro decodes with floor(v/4096) arithmetic. Resolution and the
            // pack flag are encoded on the composite's bool channels by the
            // component Lua so a monitor script can auto-decode.
            if pack {
                let n = (res_x * res_y) as usize;
                let mut i = 0usize;
                while i < n {
                    let hi = (cells[i] * 4095.0).floor();
                    let lo = if i + 1 < n {
                        (cells[i + 1] * 4095.0).floor()
                    } else {
                        0.0
                    };
                    packed_vals.push(hi * 4096.0 + lo);
                    i += 2;
                }
            }
        }

        // A scan counter for the throttled dump; in player builds nothing reads
        // it, so the binding is allow-listed rather than removed.
        #[allow(unused_variables)]
        let scan_index = SCAN_COUNT.fetch_add(1, Ordering::Relaxed);
        // Verbose diagnostics only exist in the logging build; the player build
        // compiles this whole block out, so a long session leaves a log with a
        // handful of lines instead of megabytes of scan dumps.
        #[cfg(feature = "logging")]
        if scan_index % 300 == 0 {
            {
                let ready = TEMP_READY.load(Ordering::SeqCst);
                log_line(&format!(
                    "scan: sources={} total_flux={:.5} base={} strength {:.3} \
error=({:+.6},{:+.6}) turns centroid wx/ws={:.4} wy/ws={:.4} cells_lit={} peak={:.3}",
                    source_count,
                    total_flux,
                    if ready {
                        format!("{:.1} C", base_temp)
                    } else {
                        "n/a".to_string()
                    },
                    strength,
                    az_error,
                    el_error,
                    diag_centroid.0,
                    diag_centroid.1,
                    cells.iter().filter(|v| **v > 0.0).count(),
                    cells.iter().cloned().fold(0.0f64, f64::max),
                ));
                log_line(&format!(
                    "scan boresight fwd=({:.3},{:.3},{:.3}) fov={:.1} deg steer=({:+.1},{:+.1}) deg cone=+-{:.1} deg",
                    diag_fwd.0,
                    diag_fwd.1,
                    diag_fwd.2,
                    diag_fov.to_degrees(),
                    diag_steer.0.to_degrees(),
                    diag_steer.1.to_degrees(),
                    diag_cone.to_degrees()
                ));
                for (i, s) in diag_visible.iter().enumerate().take(4) {
                    let d = s.0 - diag_steer.0;
                    let daz = d - TAU * (d / TAU).round();
                    let del = s.1 - diag_steer.1;
                    let off = ((daz * s.1.cos().max(0.2)).powi(2) + del * del).sqrt();
                    log_line(&format!(
                        "  dir[{}]{}: az={:+.2} deg el={:+.2} deg off={:.2} deg flux={:.4}",
                        i,
                        if s.4 > 0.5 { " SKY" } else { "" },
                        daz.to_degrees(),
                        del.to_degrees(),
                        off.to_degrees(),
                        s.2
                    ));
                }
                if !ready {
                    let fh = FIRES_HOLDER.load(Ordering::Relaxed);
                    if fh != 0 {
                        log_line(
                            "scan: temperature hook not fired yet; fires holder cached (flare/fire only mode)",
                        );
                    } else {
                        log_line("scan: temperature hook has not fired yet (TEMP_READY=false)");
                    }
                }
            }
            let sun_state = SUN_LOG.load(Ordering::Relaxed);
            {
                // Detailed sun diagnostics: the game's own rotating sun
                // function output, the light direction the renderer uploaded,
                // and the sky object's sun/moon vectors (scene+0x370+0x238).
                let scene = scene_env();
                if scene >= 0x10000 {
                    let settings_fn: unsafe extern "C" fn() -> usize = game_fn(RVA_SETTINGS);
                    let st = settings_fn();
                    if st >= 0x10000 {
                        let day_ticks = (read_u32_at(st) as u64) * 3600;
                        if day_ticks > 0 {
                            let clock = read_u32_at(scene + 0xbaec0) as u64;
                            let pct = (clock % day_ticks) as f64 / day_ticks as f64;
                            let sunrot: unsafe extern "C" fn(f32, *mut f32, *mut f32) =
                                game_fn(RVA_SUNROT);
                            let mut d = [0f32; 4];
                            let mut m = [0f32; 16];
                            sunrot(pct as f32, d.as_mut_ptr(), m.as_mut_ptr());
                            let (nx, ny, nz) = (-(d[0] as f64), -(d[1] as f64), -(d[2] as f64));
                            let nn = (nx * nx + ny * ny + nz * nz).sqrt().max(1.0e-9);
                            log_line(&format!(
                                "suncalc: scene={:#x} clock={} pct={:.4} hour={:.2} bf9e0=({:+.4},{:+.4},{:+.4}) to_sun el={:+.1} az={:+.1}",
                                scene,
                                clock,
                                pct,
                                pct * 24.0,
                                d[0],
                                d[1],
                                d[2],
                                (ny / nn).asin().to_degrees(),
                                (nz / nn).atan2(nx / nn).to_degrees()
                            ));
                        }
                    }
                    let sx = read_f32_at(scene + 0x5a8) as f64;
                    let sy = read_f32_at(scene + 0x5ac) as f64;
                    let sz = read_f32_at(scene + 0x5b0) as f64;
                    let mx = read_f32_at(scene + 0x5b4) as f64;
                    let my = read_f32_at(scene + 0x5b8) as f64;
                    let mz = read_f32_at(scene + 0x5bc) as f64;
                    let sn = (sx * sx + sy * sy + sz * sz).sqrt();
                    let mn = (mx * mx + my * my + mz * mz).sqrt();
                    log_line(&format!(
                        "sunsky: sun=({:+.4},{:+.4},{:+.4}) |{:.4}| el={:+.1} az={:+.1} | moon=({:+.4},{:+.4},{:+.4}) |{:.4}|",
                        sx, sy, sz, sn,
                        if sn > 1.0e-9 { (sy / sn).asin().to_degrees() } else { 0.0 },
                        if sn > 1.0e-9 { (sz / sn).atan2(sx / sn).to_degrees() } else { 0.0 },
                        mx, my, mz, mn
                    ));
                }
                if LIGHT_DIR_VALID.load(Ordering::Relaxed) {
                    let (lx, ly, lz) = {
                        let g = LIGHT_DIR.lock().unwrap_or_else(|e| e.into_inner());
                        (g[0], g[1], g[2])
                    };
                    let ln = (lx * lx + ly * ly + lz * lz).sqrt().max(1.0e-9);
                    log_line(&format!(
                        "sunlight: dir=({:+.4},{:+.4},{:+.4}) el={:+.1} az={:+.1}",
                        lx, ly, lz,
                        (ly / ln).asin().to_degrees(),
                        (lz / ln).atan2(lx / ln).to_degrees()
                    ));
                }
            }
            if sun_state == 0 {
                log_line("sun: no scene cached yet (waiting for the render uniforms hook)");
            } else if sun_state == 1 {
                log_line("sun: below the horizon");
            } else {
                let p = (sun_state - 2) as f64 / 1000.0;
                log_line(&format!(
                    "sun: day_percent={:.3} hour={:.1} emitter active el={:+.1} deg az={:+.1} deg{}",
                    p,
                    p * 24.0,
                    SUN_EL.load(Ordering::Relaxed) as f64 / 10.0,
                    SUN_AZ.load(Ordering::Relaxed) as f64 / 10.0,
                    "",
                ));
            }
            {
                let scene = SCENE.load(Ordering::Relaxed);
                let (fog, rain) = weather_fog_rain();
                if scene >= 0x10000 {
                    let w = scene + 0xbade8;
                    log_line(&format!(
                        "weather: fog={:.3} rain={:.3} (live {:.3}/{:.3}) wind={:.3} dir={:+.3} sigma={:.5}/m",
                        fog,
                        rain,
                        read_f32_at(w + 0x90),
                        read_f32_at(w + 0x98),
                        read_f32_at(w + 0xa0).max(read_f32_at(w + 0xa4)),
                        read_f32_at(w + 0xa8),
                        fog as f64 * FOG_SIGMA_PER_M + rain as f64 * RAIN_SIGMA_PER_M,
                    ));
                    log_line(&format!(
                        "weather: sun transmittance={:.5} (fog sigma {:.5}/m over {:.0} m)",
                        weather_transmittance(fog, rain, SUN_DIST, true),
                        FOG_SIGMA_SKY_PER_M,
                        SKY_PATH_CLAMP_M,
                    ));
                } else {
                    log_line("weather: scene not cached yet");
                }
            }
            log_line(&format!(
                "scan origin=({:.1},{:.1},{:.1})",
                origin.0, origin.1, origin.2
            ));
            let vcount = VARIANT_COUNT.load(Ordering::Relaxed).min(MAX_VARIANTS);
            log_line(&format!("scan: {vcount} heat container variant(s)"));
            for i in 0..vcount {
                let a = variant_args(i);
                log_line(&format!(
                    "  variant[{i}] heat={:#x} filter={:#x}",
                    a[0], a[7]
                ));
                log_heat_container(a[0]);
                log_volume_heat_sources(a[0]);
            }
            // Dump the fires holder the scan actually uses: the cached variant
            // when the temperature hook has run, the fire-render holder otherwise.
            let dump_holder = if vcount > 0 {
                variant_args(0)[3]
            } else {
                FIRES_HOLDER.load(Ordering::Relaxed)
            };
            log_fires_container(dump_holder);
        }

        let _ = lua_push_table(lua);
        if cells_out {
            if pack && !packed_vals.is_empty() {
                for (index, value) in packed_vals.iter().enumerate() {
                    lua_push_number(lua, *value);
                    lua_rawseti(lua, index as i64 + 1);
                }
            } else {
                for (index, value) in cells.iter().enumerate() {
                    lua_push_number(lua, *value);
                    lua_rawseti(lua, index as i64 + 1);
                }
            }
        }
        lua_push_number(lua, az_error);
        lua_push_number(lua, el_error);
        lua_push_number(lua, strength);
        lua_push_number(lua, total_flux);
        lua_push_number(lua, base_temp);
        lua_push_number(lua, source_count as f64);
        lua_push_number(lua, seeker_id as f64);
    }
    8
}

// ---------------------------------------------------------------------------
// vector / matrix helpers
// ---------------------------------------------------------------------------
