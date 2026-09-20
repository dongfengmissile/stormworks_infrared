//! The sensor model itself: radiometry (MWIR band radiance), atmospheric
//! attenuation, source projection onto the array, and the physics raycast
//! used for occlusion.

use std::sync::atomic::Ordering;

use crate::memory::{game_fn, read_f32_at};
use crate::offsets::*;
use crate::state::*;

/// A world-space point.
pub(crate) type Vec3 = (f64, f64, f64);

pub(crate) struct IrSource {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) z: f64,
    pub(crate) r_eff: f64, // effective emitting radius (m)
    pub(crate) flux: f64,  // irradiance at the sensor, 1.0 == 127 C / 1 m source at 10 m
    pub(crate) az: f64,    // direction in the sensor frame (rad)
    pub(crate) el: f64,
    pub(crate) theta: f64, // angular radius (rad)
    pub(crate) sky: bool,  // sky source (the sun): not range-culled
}

/// Planck spectral radiance integrated over 3..5 um (W/m^2/sr), 24-point
/// Simpson. Cheap: evaluated once per source per scan, not per cell.
pub(crate) fn band_radiance(t_k: f64) -> f64 {
    const C1: f64 = 1.19104e-16; // 2 h c^2
    const C2: f64 = 1.4388e-2; // h c / k
    if !(100.0..=4000.0).contains(&t_k) {
        return 0.0;
    }
    let (a, b) = (3e-6f64, 5e-6f64);
    let n = 24usize; // even
    let h = (b - a) / n as f64;
    let mut s = 0.0f64;
    for i in 0..=n {
        let lam = a + i as f64 * h;
        let x = C2 / (lam * t_k);
        let f = C1 / lam.powi(5) / x.exp_m1();
        let w = if i == 0 || i == n {
            1.0
        } else if i % 2 == 1 {
            4.0
        } else {
            2.0
        };
        s += f * w;
    }
    s * h / 3.0
}

/// Local weather. The game keeps its weather object on the scene at
/// scene+0xbade8 (the day clock sits just after it at +0xbaec0): the live,
/// smoothed fog factor is at +0x90 and rain at +0x98, while the cheat menu /
/// server.setWeather write target values at +0x94 / +0x9c which the live
/// values chase by 1% per tick (see FUN_1402c0810 / FUN_1402c1400). Reading
/// the max of live and target covers the transition too. Wind is at
/// +0xa0 (live) / +0xa4 (target), direction at +0xa8.
pub(crate) unsafe fn weather_fog_rain() -> (f32, f32) {
    let scene = SCENE.load(Ordering::Relaxed);
    if scene < 0x10000 {
        return (0.0, 0.0);
    }
    let w = scene + 0xbade8;
    let fog = read_f32_at(w + 0x90).max(read_f32_at(w + 0x94));
    let rain = read_f32_at(w + 0x98).max(read_f32_at(w + 0x9c));
    let fog = if fog.is_finite() { fog.clamp(0.0, 1.0) } else { 0.0 };
    let rain = if rain.is_finite() { rain.clamp(0.0, 1.0) } else { 0.0 };
    (fog, rain)
}

pub(crate) fn weather_transmittance(fog: f32, rain: f32, path_m: f64, sky: bool) -> f64 {
    let fog_sigma = if sky { FOG_SIGMA_SKY_PER_M } else { FOG_SIGMA_PER_M };
    let sigma = fog as f64 * fog_sigma + rain as f64 * RAIN_SIGMA_PER_M;
    if sigma <= 0.0 {
        return 1.0;
    }
    let path = if sky { path_m.min(SKY_PATH_CLAMP_M) } else { path_m };
    (-sigma * path).exp()
}

/// Normalisation constant derived once from the 40 C anchor (see above).
pub(crate) fn mwir_ref() -> f64 {
    static REF: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *REF.get_or_init(|| {
        band_radiance(313.15) * 0.7 * 0.7 / (10.0 * 10.0) / MWIR_ANCHOR_FLUX
    })
}

/// Express a source's direction in the sensor frame (azimuth/elevation).
pub(crate) fn project_source(
    s: &mut IrSource,
    origin: Vec3,
    forward: Vec3,
    right: Vec3,
    up: Vec3,
) {
    let dx = s.x - origin.0;
    let dy = s.y - origin.1;
    let dz = s.z - origin.2;
    let len = (dx * dx + dy * dy + dz * dz).sqrt().max(1e-6);
    let (dx, dy, dz) = (dx / len, dy / len, dz / len);
    let fx = dx * forward.0 + dy * forward.1 + dz * forward.2;
    let rx = dx * right.0 + dy * right.1 + dz * right.2;
    let ux = dx * up.0 + dy * up.1 + dz * up.2;
    s.az = rx.atan2(fx);
    s.el = ux.clamp(-1.0, 1.0).asin();
}

/// World-space raycast through the game's physics world (Bullet rayTest).
/// Returns the distance to the first hit in metres, or None.
pub(crate) unsafe fn raycast_hit(ctx: usize, origin: Vec3, target: Vec3) -> Option<f64> {
    if ctx == 0 {
        return None;
    }
    let ray_fn: unsafe extern "C" fn(usize, *const f64, *mut f64, *mut usize, u32, u32) -> u8 =
        game_fn(RVA_RAYCAST);
    let seg = [
        origin.0, origin.1, origin.2, target.0, target.1, target.2,
    ];
    if (seg[3] - seg[0]) * (seg[3] - seg[0])
        + (seg[4] - seg[1]) * (seg[4] - seg[1])
        + (seg[5] - seg[2]) * (seg[5] - seg[2])
        < 1e-6
    {
        return None;
    }
    let mut hit_point = [0.0f64; 3];
    let mut hit_object: usize = 0;
    let hit = ray_fn(
        ctx,
        seg.as_ptr(),
        hit_point.as_mut_ptr(),
        &mut hit_object,
        0x7f,
        0xff,
    );
    if hit == 0 {
        return None;
    }
    let dx = hit_point[0] - seg[0];
    let dy = hit_point[1] - seg[1];
    let dz = hit_point[2] - seg[2];
    Some((dx * dx + dy * dy + dz * dz).sqrt())
}


pub(crate) fn cross(a: Vec3, b: Vec3) -> Vec3 {
    (
        a.1 * b.2 - a.2 * b.1,
        a.2 * b.0 - a.0 * b.2,
        a.0 * b.1 - a.1 * b.0,
    )
}

pub(crate) fn normalize(v: Vec3) -> Vec3 {
    let length = (v.0 * v.0 + v.1 * v.1 + v.2 * v.2).sqrt();
    if length < 1e-12 {
        (0.0, 0.0, 0.0)
    } else {
        (v.0 / length, v.1 / length, v.2 / length)
    }
}

/// Reads the component's world transform. The 4x4 matrix is stored
/// column-major at `component + 0x1e8` (verified against `FUN_140339aa0`).
pub(crate) unsafe fn read_component_matrix(component: usize) -> [f64; 16] {
    let base = (component + COMPONENT_MATRIX_OFFSET) as *const f64;
    let mut matrix = [0.0f64; 16];
    for (index, slot) in matrix.iter_mut().enumerate() {
        *slot = *base.add(index);
    }
    matrix
}

pub(crate) unsafe fn axes_for(axis: i32, matrix: &[f64; 16]) -> (Vec3, Vec3) {
    let column = |c: usize| (matrix[c * 4], matrix[c * 4 + 1], matrix[c * 4 + 2]);
    let negate = |v: Vec3| (-v.0, -v.1, -v.2);
    let x = column(0);
    let y = column(1);
    let z = column(2);
    match axis {
        0 => (x, y),
        1 => (negate(x), y),
        2 => (y, negate(z)),
        3 => (negate(y), negate(z)),
        4 => (z, y),
        5 => (negate(z), y),
        _ => (z, y),
    }
}

// ---------------------------------------------------------------------------
// hook installation
// ---------------------------------------------------------------------------
