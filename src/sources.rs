//! Collectors that turn game state into `IrSource`s: heat records, world
//! fires, volume-heat emitters and engine-exhaust effects.


use crate::ir::{
    band_radiance, mwir_ref, weather_fog_rain, weather_transmittance, IrSource, Vec3,
};
use crate::memory::{read_f32_at, read_f64_at, read_u32_at, read_usize_at};
use crate::offsets::*;
use crate::state::*;

/// Read an ASCII string stored as {ptr @+0, len u32 @+8} at `base`.
pub(crate) unsafe fn read_pstr(base: usize, max: usize) -> Option<Vec<u8>> {
    if base < 0x10000 {
        return None;
    }
    let ptr = read_usize_at(base);
    let len = read_u32_at(base + 8) as usize;
    if ptr < 0x10000 || len == 0 || len > max {
        return None;
    }
    let mut buf = vec![0u8; len];
    std::ptr::copy_nonoverlapping(ptr as *const u8, buf.as_mut_ptr(), len);
    Some(buf)
}

/// Map an exhaust-emitter effect index to an exhaust temperature, or None when
/// the effect is not an engine plume. Indices come from the definition table
/// built by the game's effect registrar.
pub(crate) fn exhaust_index_temperature(index: u32) -> Option<f64> {
    match index {
        3 | 5 | 7 => Some(EXHAUST_T_DAMAGE), // exhaust_damage_1* - the kill field
        2 | 4 | 6 => Some(EXHAUST_T_PLUME),  // exhaust* - the visible plume
        41 | 42 => Some(EXHAUST_T_ROCKET),   // rocket, rocket_strip
        _ => None,
    }
}

/// Walk a heat container and emit every record as an IrSource, applying the
/// radiometric falloff. Records are evaluated regardless of their owner (the
/// game's own evaluation filters by owner, which would hide other vehicles).
pub(crate) unsafe fn collect_ir_sources(
    c: usize,
    origin: &Vec3,
    range: f64,
    out: &mut Vec<IrSource>,
) {
    if c < 0x10000 {
        return;
    }
    let slots = read_usize_at(c + 0x20);
    let cap = read_u32_at(c + 0x28) as usize;
    let start = read_u32_at(c + 0x2c) as usize;
    let count = read_u32_at(c + 0x30) as usize;
    if slots < 0x10000 || cap == 0 || count == 0 {
        return;
    }
    let (wx_fog, wx_rain) = weather_fog_rain();
    for i in 0..count {
        let idx = (start + i) % cap;
        let holder = read_usize_at(slots + idx * 8);
        if !(0x10000..0x7fff_ffff_ffff).contains(&holder) {
            continue;
        }
        let rec = read_usize_at(holder);
        if !(0x10000..0x7fff_ffff_ffff).contains(&rec) {
            continue;
        }
        let mode = *(rec as *const u8);
        let factor = read_f32_at(rec + 0x114) as f64;
        let temp = read_f32_at(rec + 0x118) as f64;
        if factor <= 0.0 || temp <= 0.0 {
            continue;
        }
        let (cx, cy, cz, r_eff) = if mode != 0 {
            // Oriented box: matrix at +0x88 (translation column +0xe8/+0xf0/+0xf8),
            // full extents at +0x108/+0x10c/+0x110.
            let e0 = read_f32_at(rec + 0x108) as f64;
            let e1 = read_f32_at(rec + 0x10c) as f64;
            let e2 = read_f32_at(rec + 0x110) as f64;
            (
                read_f64_at(rec + 0xe8),
                read_f64_at(rec + 0xf0),
                read_f64_at(rec + 0xf8),
                0.5 * e0.max(e1).max(e2),
            )
        } else {
            (
                read_f64_at(rec + 0x68),
                read_f64_at(rec + 0x70),
                read_f64_at(rec + 0x78),
                read_f32_at(rec + 0x108) as f64,
            )
        };
        if r_eff <= 0.001 {
            continue;
        }
        let dx = cx - origin.0;
        let dy = cy - origin.1;
        let dz = cz - origin.2;
        let dist = (dx * dx + dy * dy + dz * dz).sqrt();
        let surface = (dist - r_eff).max(1.0);
        if surface > range {
            continue;
        }
        let t_k = temp + 273.15;
        let ref_const = mwir_ref();
        let flux = factor * band_radiance(t_k) * (r_eff * r_eff)
            / (surface * surface)
            / ref_const
            * weather_transmittance(wx_fog, wx_rain, surface, false);
        out.push(IrSource {
            x: cx,
            y: cy,
            z: cz,
            r_eff,
            flux,
            az: 0.0,
            el: 0.0,
            theta: (r_eff / dist.max(0.5)).atan(),
            sky: false,
        });
    }
}

/// Fires (burning objects) as IR emitters. Container layout is what
/// FUN_14030fd60 reads as param_5: ring slots +0x11d8, cap +0x11e0, start
/// +0x11e4, count +0x11e8; each group holds a world position at +0x38..+0x48
/// and an intensity float at +0x94. Treated as a ~1 m ~1000 C emitter scaled
/// by its intensity.
pub(crate) unsafe fn collect_fire_sources(
    c: usize,
    origin: &Vec3,
    range: f64,
    out: &mut Vec<IrSource>,
) {
    if c < 0x10000 {
        return;
    }
    let slots = read_usize_at(c + 0x11d8);
    let cap = read_u32_at(c + 0x11e0) as usize;
    let start = read_u32_at(c + 0x11e4) as usize;
    let count = read_u32_at(c + 0x11e8) as usize;
    if slots < 0x10000 || cap == 0 || count == 0 {
        return;
    }
    let (wx_fog, wx_rain) = weather_fog_rain();
    let n = count.min(256);
    for i in 0..n {
        let idx = (start + i) % cap;
        let group = read_usize_at(slots + idx * 8);
        if !(0x10000..0x7fff_ffff_ffff).contains(&group) {
            continue;
        }
        let x = read_f64_at(group + 0x38);
        let y = read_f64_at(group + 0x40);
        let z = read_f64_at(group + 0x48);
        let intensity = read_f32_at(group + 0x94) as f64;
        if !(x != 0.0 || y != 0.0 || z != 0.0) || intensity <= 0.0 {
            continue;
        }
        let dx = x - origin.0;
        let dy = y - origin.1;
        let dz = z - origin.2;
        let dist = (dx * dx + dy * dy + dz * dz).sqrt();
        let surface = (dist - 1.0).max(1.0);
        if surface > range {
            continue;
        }
        let flux = intensity * band_radiance(FIRE_T_K) / (surface * surface)
            / mwir_ref()
            * weather_transmittance(wx_fog, wx_rain, surface, false);
        out.push(IrSource {
            x,
            y,
            z,
            r_eff: 1.0,
            flux,
            az: 0.0,
            el: 0.0,
            theta: (1.0 / dist.max(0.5)).atan(),
            sky: false,
        });
    }
}

/// Spherical heat emitters on the heat container's *second* ring.
/// FUN_14030fd60 reads this ring (header +0x08 slots, +0x10 cap, +0x14 start, +0x18 count;
/// slot -> holder -> record) with a (dist - radius)/5 falloff per record:
/// heat float at +0x00 (degrees C, added to ambient), radius at +0x04 and world
/// position doubles at +0x40/+0x48/+0x50. Burning vehicle components and body
/// fires land here rather than in the world fire-group ring.
pub(crate) unsafe fn collect_volume_heat_sources(
    c: usize,
    origin: &Vec3,
    range: f64,
    out: &mut Vec<IrSource>,
) {
    if c < 0x10000 {
        return;
    }
    let slots = read_usize_at(c + 0x8);
    let cap = read_u32_at(c + 0x10) as usize;
    let start = read_u32_at(c + 0x14) as usize;
    let count = read_u32_at(c + 0x18) as usize;
    if slots < 0x10000 || cap == 0 || count == 0 {
        return;
    }
    let (wx_fog, wx_rain) = weather_fog_rain();
    let n = count.min(512);
    for i in 0..n {
        let idx = (start + i) % cap;
        let holder = read_usize_at(slots + idx * 8);
        if !(0x10000..0x7fff_ffff_ffff).contains(&holder) {
            continue;
        }
        let rec = read_usize_at(holder);
        if !(0x10000..0x7fff_ffff_ffff).contains(&rec) {
            continue;
        }
        let heat = read_f32_at(rec) as f64;
        if heat <= 0.0 {
            continue;
        }
        let x = read_f64_at(rec + 0x40);
        let y = read_f64_at(rec + 0x48);
        let z = read_f64_at(rec + 0x50);
        if !(x != 0.0 || y != 0.0 || z != 0.0) {
            continue;
        }
        // Zero-radius records exist as points; give them a person-sized
        // emitting area so the flux model has something to work with.
        let r_eff = {
            let r = read_f32_at(rec + 4) as f64;
            if r > 0.05 { r } else { 0.3 }
        };
        let dx = x - origin.0;
        let dy = y - origin.1;
        let dz = z - origin.2;
        let dist = (dx * dx + dy * dy + dz * dz).sqrt();
        let surface = (dist - r_eff).max(1.0);
        if surface > range {
            continue;
        }
        let t_k = heat + 273.15;
        let flux = band_radiance(t_k) * (r_eff * r_eff)
            / (surface * surface)
            / mwir_ref()
            * weather_transmittance(wx_fog, wx_rain, surface, false);
        out.push(IrSource {
            x,
            y,
            z,
            r_eff,
            flux,
            az: 0.0,
            el: 0.0,
            theta: (r_eff / dist.max(0.5)).atan(),
            sky: false,
        });
    }
}

/// Engine exhaust emitters captured from the per-tick plume update. Every
/// propulsion component updates its exhaust emitter with the nozzle's world
/// position and a throttle-scaled emission rate each tick, so the spectrum
/// follows throttle without touching game state.
pub(crate) unsafe fn collect_emitter_sources(origin: &Vec3, range: f64, out: &mut Vec<IrSource>) {
    let pending = match EMITTER_PENDING.lock() {
        Ok(mut p) => std::mem::take(&mut *p),
        Err(_) => return,
    };
    if pending.is_empty() {
        return;
    }
    let (wx_fog, wx_rain) = weather_fog_rain();
    for (_emitter, x, y, z, size, temp, block_m) in pending {
        let dx = x - origin.0;
        let dy = y - origin.1;
        let dz = z - origin.2;
        let dist = (dx * dx + dy * dy + dz * dz).sqrt();
        // Plume radius follows the block size (a 1x1 booster must not radiate
        // like a 3x3 motor), narrowed at low throttle.
        let r_eff = (ENGINE_R_BLOCK_FACTOR * block_m * (0.5 + 0.5 * size.min(1.0)))
            .clamp(ENGINE_R_MIN_M, ENGINE_R_MAX_M);
        let surface = (dist - r_eff).max(1.0);
        if surface > range {
            continue;
        }
        // One emitter can spawn a fresh instance every frame; collapse repeats.
        let dup = out.iter().any(|s| {
            let ddx = s.x - x;
            let ddy = s.y - y;
            let ddz = s.z - z;
            ddx * ddx + ddy * ddy + ddz * ddz < 4.0
        });
        if dup {
            continue;
        }
        let flux = band_radiance(temp + 273.15) * (r_eff * r_eff) / (surface * surface)
            / mwir_ref()
            * weather_transmittance(wx_fog, wx_rain, surface, false);
        out.push(IrSource {
            x,
            y,
            z,
            r_eff,
            flux,
            az: 0.0,
            el: 0.0,
            theta: (r_eff / dist.max(0.5)).atan(),
            sky: false,
        });
    }
}
