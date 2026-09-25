//! The game's heat model: the heat container variants seen by the hooked
//! temperature function, the per-record influence falloff, and the ambient
//! base temperature. Also the verbose container dumps.

use std::sync::atomic::Ordering;

use crate::ir::Vec3;
#[cfg(feature = "logging")]
use crate::logging::log_line;
use crate::memory::{game_fn, read_f32_at, read_f64_at, read_u32_at, read_usize_at};
#[cfg(feature = "logging")]
use crate::memory::readable;
use crate::offsets::*;
use crate::state::*;

/// Diagnostic: describe the cached heat-source container (ring list) and a few
/// records, so we can tell whether a nearby engine is even a heat-field source.
/// Diagnostic dump of one heat container variant (logging builds only).
#[cfg(feature = "logging")]
pub(crate) unsafe fn log_heat_container(c: usize) {
    if DEEP_DUMP_DISABLED.load(Ordering::Relaxed) {
        return;
    }
    // c can be a stale captured hook argument and every pointer below is
    // game-controlled, so EVERY dereference must be readable()-guarded. A
    // range check alone passes on any garbage pointer inside userland.
    if c < 0x10000 || !readable(c, 0x34) {
        log_line("heat container: NULL or unreadable");
        return;
    }
    let slots = read_usize_at(c + 0x20);
    let cap = read_u32_at(c + 0x28);
    let start = read_u32_at(c + 0x2c);
    let count = read_u32_at(c + 0x30);
    log_line(&format!(
        "heat container {:#x}: slots={:#x} cap={} start={} count={}",
        c, slots, cap, start, count
    ));
    if slots < 0x10000 || cap == 0 || count == 0 {
        return;
    }
    let dump = count.min(32);
    for i in 0..dump {
        let idx = ((start as usize + i as usize) % cap as usize) as usize;
        if !readable(slots + idx * 8, 8) {
            continue;
        }
        let holder = read_usize_at(slots + idx * 8);
        if !(0x10000..0x7fff_ffff_ffff).contains(&holder) || !readable(holder, 8) {
            continue;
        }
        let rec = read_usize_at(holder);
        if !(0x10000..0x7fff_ffff_ffff).contains(&rec) || !readable(rec, 0x128) {
            continue;
        }
        let mode = *(rec as *const u8);
        let cx = read_f64_at(rec + 0x68);
        let cy = read_f64_at(rec + 0x70);
        let cz = read_f64_at(rec + 0x78);
        let radius = read_f32_at(rec + 0x108);
        let ext1 = read_f32_at(rec + 0x10c);
        let ext2 = read_f32_at(rec + 0x110);
        let factor = read_f32_at(rec + 0x114);
        let temp = read_f32_at(rec + 0x118);
        let owner = read_usize_at(rec + 0x120);
        if mode != 0 {
            // Box mode: print all three full extents (e0 is at +0x108) so the
            // component type can be fingerprinted from the box shape.
            log_line(&format!(
                "  heat[{idx}] mode={mode} c=({cx:.1},{cy:.1},{cz:.1}) ext=({radius:.2},{ext1:.2},{ext2:.2}) f={factor:.3} scale={temp:.1} owner={owner:#x}"
            ));
        } else {
            log_line(&format!(
                "  heat[{idx}] mode={mode} c=({cx:.1},{cy:.1},{cz:.1}) r={radius:.2} f={factor:.3} scale={temp:.1} owner={owner:#x}"
            ));
        }
    }
}

pub(crate) fn cache_variant(args: &[usize; 8]) {
    if args[0] == 0 {
        return;
    }
    let count = VARIANT_COUNT.load(Ordering::Relaxed).min(MAX_VARIANTS);
    for i in 0..count {
        let base = i * 8;
        if VARIANT_ARGS[base].load(Ordering::Relaxed) == args[0] {
            for j in 0..8 {
                VARIANT_ARGS[base + j].store(args[j], Ordering::Relaxed);
            }
            return;
        }
    }
    let slot = count % MAX_VARIANTS;
    let base = slot * 8;
    for j in 0..8 {
        VARIANT_ARGS[base + j].store(args[j], Ordering::Relaxed);
    }
    if count < MAX_VARIANTS {
        VARIANT_COUNT.store(count + 1, Ordering::Relaxed);
    }
}

pub(crate) fn variant_args(i: usize) -> [usize; 8] {
    let base = i * 8;
    let mut a = [0usize; 8];
    for j in 0..8 {
        a[j] = VARIANT_ARGS[base + j].load(Ordering::Relaxed);
    }
    a
}

/// Heat-source influence at a point, replicated from `FUN_14030bd10` so the
/// scan does not depend on calling that function directly:
///   sphere: t = (dist(center,+0x68..) - radius(+0x108)) / (T(+0x118) * 0.125)
///   box:    t = max(0, axis dist - extent/2) / (T * 0.125)
///   return: (1 - clamp(t)) * factor(+0x114)
#[inline]
pub(crate) unsafe fn influence_at(rec: usize, px: f64, py: f64, pz: f64) -> f32 {
    let factor = read_f32_at(rec + 0x114);
    let temp = read_f32_at(rec + 0x118);
    if factor <= 0.0 || temp <= 0.0 {
        return 0.0;
    }
    let denom = temp * 0.125;
    let mode = *(rec as *const u8);
    let t = if mode != 0 {
        let a = ((px * read_f64_at(rec + 0x88)
            + py * read_f64_at(rec + 0xa8)
            + pz * read_f64_at(rec + 0xc8)
            + read_f64_at(rec + 0xe8))
            .abs() as f32
            - read_f32_at(rec + 0x108) * 0.5)
            .max(0.0);
        let b = ((px * read_f64_at(rec + 0x90)
            + py * read_f64_at(rec + 0xb0)
            + pz * read_f64_at(rec + 0xd0)
            + read_f64_at(rec + 0xf0))
            .abs() as f32
            - read_f32_at(rec + 0x10c) * 0.5)
            .max(0.0);
        let c = ((px * read_f64_at(rec + 0x98)
            + py * read_f64_at(rec + 0xb8)
            + pz * read_f64_at(rec + 0xd8)
            + read_f64_at(rec + 0xf8))
            .abs() as f32
            - read_f32_at(rec + 0x110) * 0.5)
            .max(0.0);
        a.max(b).max(c) / denom
    } else {
        let dx = px - read_f64_at(rec + 0x68);
        let dy = py - read_f64_at(rec + 0x70);
        let dz = pz - read_f64_at(rec + 0x78);
        let dist = ((dx * dx + dy * dy + dz * dz).sqrt()) as f32;
        (dist - read_f32_at(rec + 0x108)) / denom
    };
    (1.0 - t.clamp(0.0, 1.0)) * factor
}

/// Max heat contribution over every record in one container, ignoring the
/// per-owner filter: `influence(record, point) * record.temperature`.
pub(crate) unsafe fn max_heat_in_container(c: usize, px: f64, py: f64, pz: f64) -> f32 {
    if c < 0x10000 {
        return 0.0;
    }
    let slots = read_usize_at(c + 0x20);
    let cap = read_u32_at(c + 0x28) as usize;
    let start = read_u32_at(c + 0x2c) as usize;
    let count = read_u32_at(c + 0x30) as usize;
    if slots < 0x10000 || cap == 0 || count == 0 {
        return 0.0;
    }
    let mut best = 0.0f32;
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
        let influence = influence_at(rec, px, py, pz);
        if influence > 0.0 {
            let contrib = influence * read_f32_at(rec + 0x118);
            if contrib > best {
                best = contrib;
            }
        }
    }
    best
}

pub(crate) unsafe fn temperature_at(x: f64, y: f64, z: f64) -> f32 {
    if !TEMP_READY.load(Ordering::SeqCst) {
        return 20.0;
    }
    let point = [x, y, z];
    let pptr = point.as_ptr() as usize;

    // The game's own function, for fires / lava / its exact blend.
    let mut best = 20.0f32;
    let original = ORIG_TEMPERATURE.load(Ordering::SeqCst);
    let count = VARIANT_COUNT.load(Ordering::Relaxed).min(MAX_VARIANTS);
    if original != 0 && count > 0 {
        let f: unsafe extern "C" fn(
            usize,
            usize,
            usize,
            usize,
            usize,
            usize,
            usize,
            usize,
            usize,
        ) -> f32 = std::mem::transmute(original);
        let a = variant_args(0);
        best = f(pptr, a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7]);
    }

    if count > 0 {
        let base_fn: unsafe extern "C" fn(usize, usize, usize, usize, u8) -> f32 =
            game_fn(RVA_BASE_TEMP);
        let a0 = variant_args(0);
        let base = base_fn(pptr, a0[1], a0[2], a0[2], a0[6] as u8);
        let mut heat = 0.0f32;
        for i in 0..count {
            let a = variant_args(i);
            let h = max_heat_in_container(a[0], x, y, z);
            if h > heat {
                heat = h;
            }
        }
        let combined = base + heat;
        if combined > best {
            best = combined;
        }
    }
    best
}

// ---------------------------------------------------------------------------
// hooks
// ---------------------------------------------------------------------------

/// Base ambient temperature at a world point (no heat sources added).
pub(crate) unsafe fn base_ambient_at(origin: Vec3) -> f32 {
    if !TEMP_READY.load(Ordering::SeqCst) {
        return 20.0;
    }
    if VARIANT_COUNT.load(Ordering::Relaxed) == 0 {
        return 20.0;
    }
    let a0 = variant_args(0);
    let base_fn: unsafe extern "C" fn(usize, usize, usize, usize, u8) -> f32 =
        game_fn(RVA_BASE_TEMP);
    let point = [origin.0, origin.1, origin.2];
    base_fn(
        point.as_ptr() as usize,
        a0[1],
        a0[2],
        a0[2],
        a0[6] as u8,
    )
}

/// Diagnostic dump of the fires container (logging builds only).
#[cfg(feature = "logging")]
pub(crate) unsafe fn log_fires_container(c: usize) {
    if DEEP_DUMP_DISABLED.load(Ordering::Relaxed) {
        return;
    }
    if c < 0x10000 || !readable(c, 0x11ec) {
        log_line("fires container: NULL or unreadable");
        return;
    }
    let slots = read_usize_at(c + 0x11d8);
    let cap = read_u32_at(c + 0x11e0);
    let start = read_u32_at(c + 0x11e4);
    let count = read_u32_at(c + 0x11e8);
    log_line(&format!(
        "fires container {:#x}: slots={:#x} cap={} start={} count={}",
        c, slots, cap, start, count
    ));
    if slots < 0x10000 || cap == 0 || count == 0 {
        return;
    }
    let dump = (count as usize).min(4);
    for i in 0..dump {
        let idx = ((start as usize + i) % cap as usize) as usize;
        if !readable(slots + idx * 8, 8) {
            continue;
        }
        let group = read_usize_at(slots + idx * 8);
        if !(0x10000..0x7fff_ffff_ffff).contains(&group) || !readable(group, 0x98) {
            continue;
        }
        let x = read_f64_at(group + 0x38);
        let y = read_f64_at(group + 0x40);
        let z = read_f64_at(group + 0x48);
        let intensity = read_f32_at(group + 0x94);
        log_line(&format!(
            "  fire[{}] c=({:.1},{:.1},{:.1}) intensity={:.3}",
            idx, x, y, z, intensity
        ));
    }
}

/// Diagnostic dump of the heat container's second ring (volume heat emitters).
#[cfg(feature = "logging")]
pub(crate) unsafe fn log_volume_heat_sources(c: usize) {
    if DEEP_DUMP_DISABLED.load(Ordering::Relaxed) {
        return;
    }
    if c < 0x10000 || !readable(c, 0x1c) {
        return;
    }
    let slots = read_usize_at(c + 0x8);
    let cap = read_u32_at(c + 0x10);
    let start = read_u32_at(c + 0x14);
    let count = read_u32_at(c + 0x18);
    log_line(&format!(
        "volume-heat container {:#x}: slots={:#x} cap={} start={} count={}",
        c, slots, cap, start, count
    ));
    if slots < 0x10000 || cap == 0 || count == 0 {
        return;
    }
    let dump = (count as usize).min(6);
    for i in 0..dump {
        let idx = ((start as usize + i) % cap as usize) as usize;
        if !readable(slots + idx * 8, 8) {
            continue;
        }
        let holder = read_usize_at(slots + idx * 8);
        if !(0x10000..0x7fff_ffff_ffff).contains(&holder) || !readable(holder, 8) {
            continue;
        }
        let rec = read_usize_at(holder);
        if !(0x10000..0x7fff_ffff_ffff).contains(&rec) || !readable(rec, 0x58) {
            continue;
        }
        log_line(&format!(
            "  vheat[{}] c=({:.1},{:.1},{:.1}) heat={:.1} C radius={:.2}",
            idx,
            read_f64_at(rec + 0x40),
            read_f64_at(rec + 0x48),
            read_f64_at(rec + 0x50),
            read_f32_at(rec),
            read_f32_at(rec + 4)
        ));
    }
}
