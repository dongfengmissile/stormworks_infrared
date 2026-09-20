//! The sun as an IR emitter: the game's own rotating sun direction is
//! captured from the per-tick sun update, and the day/night gate comes from
//! the live daylight factor.

use std::sync::atomic::Ordering;

use crate::ir::{weather_fog_rain, weather_transmittance, IrSource, Vec3};
use crate::heat::variant_args;
use crate::memory::{game_fn, read_u32_at};
use crate::offsets::*;
use crate::state::*;

pub(crate) unsafe fn scene_env() -> usize {
    let s = SCENE.load(Ordering::Relaxed);
    if s >= 0x10000 {
        return s;
    }
    if VARIANT_COUNT.load(Ordering::Relaxed) > 0 {
        let v = variant_args(0)[1];
        if v >= 0x10000 {
            return v;
        }
    }
    FIRES_HOLDER.load(Ordering::Relaxed) // last resort (has fires, but no scene clock)
}

pub(crate) unsafe fn add_sun_source(origin: Vec3, sources: &mut Vec<IrSource>) {
    SUN_LOG.store(0, Ordering::Relaxed);
    let env = scene_env();
    if env < 0x10000 {
        return; // no scene yet (dump prints "waiting")
    }
    let settings_fn: unsafe extern "C" fn() -> usize = game_fn(RVA_SETTINGS);
    let settings = settings_fn();
    if settings < 0x10000 {
        return;
    }
    let day_min = read_u32_at(settings) as u64;
    if day_min == 0 || day_min > 24 * 60 {
        return;
    }
    let day_ticks = day_min * 3600;
    let clock = read_u32_at(env + 0xbaec0) as u64;
    let percent = (clock % day_ticks) as f64 / day_ticks as f64;

    // Camera-independent sun direction straight from the game: FUN_1402bf9e0
    // is the exact function that builds the rotating sun (it feeds the sun
    // light matrix, the sky sprite at 18 km and the 37449100 m sun ray used
    // by the sky/atmosphere code). Pure math, no game state — safe to call.
    let sunrot: unsafe extern "C" fn(f32, *mut f32, *mut f32) = game_fn(RVA_SUNROT);
    let mut gdir = [0f32; 4];
    let mut gmat = [0f32; 16];
    sunrot(percent as f32, gdir.as_mut_ptr(), gmat.as_mut_ptr());
    let vx = gdir[0] as f64;
    let vy = gdir[1] as f64;
    let vz = gdir[2] as f64;
    let vn = (vx * vx + vy * vy + vz * vz).sqrt();
    if vn < 1.0e-6 {
        SUN_LOG.store(0, Ordering::Relaxed);
        return;
    }
    // The game's vector points FROM the sun (the light travels along it, and
    // the sun ray origin is camera - dir * 37449100); the seeker needs the
    // direction TO the sun.
    let (dx, dy, dz) = (-vx / vn, -vy / vn, -vz / vn);
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
    SUN_SCENE_LOG.store(env, Ordering::Relaxed);
    SUN_CLOCK_LOG.store(clock as usize, Ordering::Relaxed);
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
    });
    SUN_LOG.store((percent * 1000.0).clamp(0.0, 1_000_000.0) as usize + 2, Ordering::Relaxed);
}
