//! The sun as an IR emitter: the game's own rotating sun direction is
//! captured from the per-tick sun update, and the day/night gate comes from
//! the live daylight factor.

use std::sync::atomic::Ordering;

use crate::ir::{weather_fog_rain, weather_transmittance, IrSource, Vec3};
use crate::memory::{game_fn, read_u32_at, readable};
use crate::offsets::*;
use crate::state::*;

/// Day clock (uint32 minutes-ish) inside the scene object.
const SCENE_DAY_CLOCK: usize = 0xbaec0;

/// The cached scene object, or the fires holder while no scene is cached yet.
/// The fires holder has no clock, so the caller guards every field it reads:
/// nothing derived from a hook argument is used as a scene any more (that value
/// was the wind field, not the scene).
pub(crate) unsafe fn scene_env() -> usize {
    let s = SCENE.load(Ordering::Relaxed);
    if s >= 0x10000 {
        return s;
    }
    FIRES_HOLDER.load(Ordering::Relaxed) // last resort (has fires, but no scene clock)
}

/// The game's sun: (unit vector TO the sun, day percent 0..1), or None while
/// no scene/clock is available. Game thread only (reads the scene clock).
pub(crate) unsafe fn game_sun() -> Option<([f64; 3], f64)> {
    let env = scene_env();
    if env < 0x10000 {
        return None;
    }
    let settings_fn: unsafe extern "C" fn() -> usize = game_fn(RVA_SETTINGS);
    let settings = settings_fn();
    if settings < 0x10000 {
        return None;
    }
    let day_min = read_u32_at(settings) as u64;
    if day_min == 0 || day_min > 24 * 60 {
        return None;
    }
    let day_ticks = day_min * 3600;
    // The fires-holder fallback has no clock: skip instead of reading wild
    // memory at the scene offset.
    if !readable(env + SCENE_DAY_CLOCK, 4) {
        return None;
    }
    let clock = read_u32_at(env + SCENE_DAY_CLOCK) as u64;
    let percent = (clock % day_ticks) as f64 / day_ticks as f64;
    // Camera-independent sun direction straight from the game: FUN_1402bf9e0
    // is the exact function that builds the rotating sun (it feeds the sun
    // light matrix, the sky sprite at 18 km and the 37449100 m sun ray used
    // by the sky/atmosphere code). Pure math, no game state - safe to call.
    let sunrot: unsafe extern "C" fn(f32, *mut f32, *mut f32) = game_fn(RVA_SUNROT);
    let mut gdir = [0f32; 4];
    let mut gmat = [0f32; 16];
    sunrot(percent as f32, gdir.as_mut_ptr(), gmat.as_mut_ptr());
    let (vx, vy, vz) = (gdir[0] as f64, gdir[1] as f64, gdir[2] as f64);
    let vn = (vx * vx + vy * vy + vz * vz).sqrt();
    if !(vn > 1.0e-6) {
        return None;
    }
    // The game's vector points FROM the sun (the light travels along it, and
    // the sun ray origin is camera - dir * 37449100); we want TO the sun.
    SUN_SCENE_LOG.store(env, Ordering::Relaxed);
    SUN_CLOCK_LOG.store(clock as usize, Ordering::Relaxed);
    Some(([-vx / vn, -vy / vn, -vz / vn], percent))
}

/// The game draws its sun sprite lower than its light direction: measured in
/// game (08/12/16 h) as to_sun shifted down by this much before normalising
/// (1.85 deg low at 65 deg elevation, ~3.5 deg at 27 deg, none sideways).
/// shortcut: empirical fit from screenshots, replace with the sprite's real
/// placement once the sun draw is reverse engineered.
pub(crate) const SUN_SPRITE_DROP: f64 = 0.07;

/// Where the player sees the sun, from the game's to-sun light direction.
pub(crate) fn visible_sun_dir(to_sun: [f64; 3]) -> [f64; 3] {
    let v = [to_sun[0], to_sun[1] - SUN_SPRITE_DROP, to_sun[2]];
    let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    [v[0] / n, v[1] / n, v[2] / n]
}

pub(crate) unsafe fn add_sun_source(origin: Vec3, sources: &mut Vec<IrSource>) {
    SUN_LOG.store(0, Ordering::Relaxed);
    let Some(([dx, dy, dz], percent)) = game_sun() else {
        return; // no scene yet (dump prints "waiting")
    };
    // The seeker sees the sun where it is drawn.
    let [dx, dy, dz] = visible_sun_dir([dx, dy, dz]);
    let el = dy.asin();
    if el < 0.0 {
        SUN_LOG.store(1, Ordering::Relaxed); // 1 = sun below the horizon
        return;
    }
    let az_deg = dz.atan2(dx).to_degrees();
    SUN_EL.store((el.to_degrees() * 10.0) as isize, Ordering::Relaxed);
    SUN_AZ.store((az_deg * 10.0) as isize, Ordering::Relaxed);
    SUN_PCT.store(
        (percent * 1000.0).clamp(0.0, 1_000_000.0) as usize,
        Ordering::Relaxed,
    );
    let (wx_fog, wx_rain) = weather_fog_rain();
    sources.push(IrSource {
        x: origin.0 + dx * SUN_DIST,
        y: origin.1 + dy * SUN_DIST,
        z: origin.2 + dz * SUN_DIST,
        r_eff: SUN_R_EFF,
        flux: SUN_FLUX * weather_transmittance(wx_fog, wx_rain, SUN_DIST, true),
        az: 0.0,
        el: 0.0,
        theta: (SUN_R_EFF / SUN_DIST).atan(),
        sky: true,
        temp_c: 0.0,
    });
    SUN_LOG.store((percent * 1000.0).clamp(0.0, 1_000_000.0) as usize + 2, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_sun_matches_measured_drop() {
        let el = |d: [f64; 3]| d[1].asin().to_degrees();
        let at = |deg: f64| [deg.to_radians().cos(), deg.to_radians().sin(), 0.0];
        let noon = el(at(65.4)) - el(visible_sun_dir(at(65.4)));
        let morning = el(at(27.5)) - el(visible_sun_dir(at(27.5)));
        assert!((noon - 1.85).abs() < 0.3, "{noon}");
        assert!((morning - 3.5).abs() < 0.5, "{morning}");
    }
}
