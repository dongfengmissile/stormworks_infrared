//! Every MinHook detour: the component Lua registrar, the temperature
//! function, the fire vertex builder, the scene tick, the sun update, the
//! render uniform fill and the exhaust-plume emitters — plus the fail-closed prologue
//! gate that refuses to install when the game build changed.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use minhook::MinHook;

use crate::api::{register_ir_screen_table, register_ir_table};
use crate::heat::cache_variant;
#[cfg(feature = "logging")]
use crate::heat::{log_fires_container, log_heat_container, log_volume_heat_sources};
use crate::logging::log_line;
use crate::memory::{bytes_match, read_f32_at, read_f64_at, read_u32_at, read_usize_at, readable};
use crate::offsets::*;
#[cfg(feature = "logging")]
use crate::sources::read_pstr;
use crate::sources::exhaust_index_temperature;
use crate::state::*;

pub(crate) extern "C" fn ir_registrar_hook(context: usize) {
    let original = ORIG_REGISTRAR.load(Ordering::SeqCst);
    if original != 0 {
        let f: unsafe extern "C" fn(usize) = unsafe { std::mem::transmute(original) };
        unsafe { f(context) };
    }
    unsafe { register_ir_table(context) };
    REGISTRAR_HITS.fetch_add(1, Ordering::Relaxed);
}

/// Screen / microcontroller Lua initializer (it builds `debug`, `async`,
/// `screen`).
///
/// `ir` is registered here as well as in the component-mod environment, because
/// a microcontroller script is where `screen.*` lives and the image therefore
/// has to be drawn from there. Only the component-independent calls are
/// exposed - see register_ir_screen_table.
pub(crate) extern "C" fn ir_screen_init_hook(state: usize) {
    let original = ORIG_SCREEN_INIT.load(Ordering::SeqCst);
    if original != 0 {
        let f: unsafe extern "C" fn(usize) = unsafe { std::mem::transmute(original) };
        unsafe { f(state) };
    }
    unsafe { register_ir_screen_table(state) };
    SCREEN_INIT_HITS.fetch_add(1, Ordering::Relaxed);
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
#[allow(clippy::too_many_arguments)]
extern "C" fn ir_exhaust_update_hook(
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
) {
    unsafe {
        if readable(emitter, 0x18) {
            let definition = read_usize_at(emitter);
            if definition >= 0x10000 && readable(definition, 0x68) {
                let index = read_u32_at(definition + 0x10);
                // The rate arrives in a stack slot. Callers do pass it, but a
                // caller that does not would leave an arbitrary value there, so
                // probe before reading rather than trusting the ABI.
                let rate = if readable(rate_ptr, 4) {
                    read_f32_at(rate_ptr) as f64
                } else {
                    0.0
                };
                let positioned = readable(position, 24);

                #[cfg(feature = "logging")]
                {
                    use std::sync::atomic::AtomicUsize;
                    static UPDATE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
                    let n = UPDATE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                    if n < 8 {
                        log_line(&format!(
                            "exhaust probe[{}] emitter={:#x} index={} rate={:.4} pos=({:.1},{:.1},{:.1})",
                            n, emitter, index, rate,
                            read_f64_at(position), read_f64_at(position + 8), read_f64_at(position + 0x10)
                        ));
                    }
                    // One line per distinct effect, with its name, so the log
                    // shows exactly which plumes the game is running.
                    let first_sighting = match SEEN_INDICES.lock() {
                        Ok(mut seen) => {
                            if seen.contains(&index) {
                                false
                            } else if seen.len() < 32 {
                                seen.push(index);
                                true
                            } else {
                                false
                            }
                        }
                        Err(_) => false,
                    };
                    if first_sighting {
                        let name = read_pstr(definition, 80)
                            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                            .unwrap_or_else(|| "<unreadable>".to_string());
                        log_line(&format!(
                            "exhaust index {index} first seen: name=\"{name}\" emitter={emitter:#x} rate={rate:.4} pos=({:.1},{:.1},{:.1})",
                            read_f64_at(position), read_f64_at(position + 8), read_f64_at(position + 0x10)
                        ));
                    }
                }

                if let Some(temp) = exhaust_index_temperature(index) {
                    if rate <= EMITTER_MIN_RATE {
                        // The plume has stopped: drop the source rather than
                        // leaving a phantom at the last position it reported.
                        if let Ok(mut pending) = EMITTER_PENDING.lock() {
                            pending.retain(|entry| entry.0 != emitter);
                        }
                    }
                    if rate > EMITTER_MIN_RATE && positioned {
                        let x = read_f64_at(position);
                        let y = read_f64_at(position + 8);
                        let z = read_f64_at(position + 0x10);
                        if x != 0.0 || y != 0.0 || z != 0.0 {
                            if let Ok(mut pending) = EMITTER_PENDING.lock() {
                                if let Some(entry) = pending.iter_mut().find(|e| e.0 == emitter) {
                                    *entry = (
                                        emitter,
                                        now_ms(),
                                        x,
                                        y,
                                        z,
                                        rate,
                                        temp,
                                        ENGINE_DEFAULT_SIZE_M,
                                    );
                                } else if pending.len() < EMITTER_MAX_PENDING {
                                    pending.push((
                                        emitter,
                                        now_ms(),
                                        x,
                                        y,
                                        z,
                                        rate,
                                        temp,
                                        ENGINE_DEFAULT_SIZE_M,
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
        let original = ORIG_EXHAUST_UPDATE.load(Ordering::SeqCst);
        if original == 0 {
            return;
        }
        let f: unsafe extern "C" fn(
            usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize,
        ) = std::mem::transmute(original);
        f(emitter, position, p3, p4, rate_ptr, p6, p7, p8, p9, p10, p11);
    }
}

/// World position of a component, from a tick's world matrix (arg 5) and origin
/// offset (arg 6): the game computes voxel*0.25 - origin and transforms that by
/// the component's 3x4 world matrix. Reproduced here so the IR source sits on
/// the block itself.
unsafe fn component_world_position(
    component: usize,
    matrix: usize,
    origin: usize,
) -> Option<(f64, f64, f64)> {
    if !readable(component, 0x24) || !readable(matrix, 0x90) || !readable(origin, 12) {
        return None;
    }
    let dx = read_u32_at(component + 0x18) as i32 as f64 * 0.25 - read_f32_at(origin) as f64;
    let dy = read_u32_at(component + 0x1c) as i32 as f64 * 0.25 - read_f32_at(origin + 4) as f64;
    let dz = read_u32_at(component + 0x20) as i32 as f64 * 0.25 - read_f32_at(origin + 8) as f64;
    Some((
        read_f64_at(matrix + 0x18) * dx
            + read_f64_at(matrix + 0x38) * dy
            + read_f64_at(matrix + 0x58) * dz
            + read_f64_at(matrix + 0x78),
        read_f64_at(matrix + 0x20) * dx
            + read_f64_at(matrix + 0x40) * dy
            + read_f64_at(matrix + 0x60) * dz
            + read_f64_at(matrix + 0x80),
        read_f64_at(matrix + 0x28) * dx
            + read_f64_at(matrix + 0x48) * dy
            + read_f64_at(matrix + 0x68) * dz
            + read_f64_at(matrix + 0x88),
    ))
}

/// Track one engine-family component for the periodic diagnostics.
fn record_engine_sample(
    key: usize,
    family: &'static str,
    power: f64,
    heat: f64,
    position: Option<(f64, f64, f64)>,
    size_m: f64,
) -> u32 {
    let Ok(mut samples) = ENGINE_SAMPLES.lock() else {
        return 0;
    };
    if let Some(sample) = samples.iter_mut().find(|sample| sample.key == key) {
        sample.max_power = sample.max_power.max(power);
        sample.max_heat = sample.max_heat.max(heat);
        sample.calls = sample.calls.saturating_add(1);
        sample.size_m = size_m;
        if let Some((x, y, z)) = position {
            sample.x = x;
            sample.y = y;
            sample.z = z;
        }
        return sample.calls;
    } else if samples.len() < 32 {
        let (x, y, z) = position.unwrap_or((0.0, 0.0, 0.0));
        samples.push(crate::state::EngineSample {
            key,
            family,
            max_power: power,
            max_heat: heat,
            x,
            y,
            z,
            calls: 1,
            size_m,
            progress: 0.0,
            propellant: 0.0,
            drain: 0.0,
            burning: false,
        });
    }
    1
}

/// Update the rocket-only fields of a tracked component.
unsafe fn record_rocket_state(
    key: usize,
    progress: f32,
    propellant: f32,
    burning: bool,
) -> f32 {
    let Ok(mut samples) = ENGINE_SAMPLES.lock() else {
        return 0.0;
    };
    if let Some(sample) = samples.iter_mut().find(|sample| sample.key == key) {
        // Either counter dropping means the motor is consuming propellant.
        let drain = (sample.progress - progress)
            .max(sample.propellant - propellant)
            .max(0.0);
        if drain > sample.drain {
            sample.drain = drain;
        }
        sample.progress = progress;
        sample.propellant = propellant;
        sample.burning = burning;
        return drain;
    }
    0.0
}

/// Best-effort part name for a component definition. Part names such as
/// `solid_rocket_nozzle_large` live on the definition, so scanning its first
/// bytes for a pointer to an identifier-like string labels each component.
#[cfg(feature = "logging")]
unsafe fn definition_name(definition: usize) -> String {
    if definition < 0x10000 || !readable(definition, 0x200) {
        return String::from("?");
    }
    // Short names live inline in the std::string buffer (SSO), so scan for
    // inline runs as well as following pointers.
    let mut run = Vec::new();
    for offset in 0..0x300 {
        // Re-verify each chunk: the 0x200-byte check above does not cover this
        // whole 0x300-byte scan, and one dereference past an unmapped page
        // takes the game down with it.
        if offset % 64 == 0 && !readable(definition + offset, 64) {
            break;
        }
        let byte = *((definition + offset) as *const u8);
        if (0x20..0x7f).contains(&byte) {
            run.push(byte);
            continue;
        }
        if run.len() >= 4 {
            let text = String::from_utf8_lossy(&run).to_string();
            if text.chars().any(|c| c.is_ascii_alphabetic())
                && !text.contains('/')
                && !text.contains('.')
            {
                return text;
            }
        }
        run.clear();
    }
    for offset in (0..0x200).step_by(8) {
        let pointer = read_usize_at(definition + offset);
        if pointer < 0x10000 || !readable(pointer, 48) {
            continue;
        }
        let mut bytes = Vec::new();
        for index in 0..48 {
            let byte = *((pointer + index) as *const u8);
            if byte == 0 {
                break;
            }
            if !(0x20..0x7f).contains(&byte) {
                bytes.clear();
                break;
            }
            bytes.push(byte);
        }
        if bytes.len() < 4 || !bytes.contains(&b'_') {
            continue;
        }
        // The definition holds both part names ("Modular Engine Cylinder 1x1")
        // and asset paths ("audio/fx/engine_medium_louder.ogg"), and the latter
        // contains an underscore too, so require a printable, letter-bearing
        // string with no path separator or extension.
        if bytes.iter().any(|byte| !(0x20..0x7f).contains(byte))
            || !bytes.iter().any(|byte| byte.is_ascii_alphabetic())
            || bytes.contains(&b'/')
            || bytes.contains(&b'.')
        {
            continue;
        }
        if let Ok(text) = String::from_utf8(bytes) {
            return text;
        }
    }
    String::from("?")
}

/// Physical size of a component, from its definition's voxel bounds. The
/// middle extent is the block's diameter for both short parts (solid fuel
/// segments) and long ones (liquid motors), so it is a good plume proxy.
unsafe fn component_size_m(component: usize) -> f64 {
    let definition = read_usize_at(component + 0x58);
    if definition < 0x10000 || !readable(definition, DEF_VOXEL_MAX + 12) {
        return ENGINE_DEFAULT_SIZE_M;
    }
    let mut extents = [0.0f64; 3];
    for axis in 0..3 {
        let low_int = read_u32_at(definition + DEF_VOXEL_MIN + axis * 4) as i32;
        let high_int = read_u32_at(definition + DEF_VOXEL_MAX + axis * 4) as i32;
        let low_float = read_f32_at(definition + DEF_VOXEL_MIN + axis * 4) as f64;
        let high_float = read_f32_at(definition + DEF_VOXEL_MAX + axis * 4) as f64;
        // The parser may store the bounds as integers or floats; accept
        // whichever encoding gives a sane block extent.
        let voxels = if (1..=32).contains(&(high_int - low_int + 1)) {
            (high_int - low_int + 1) as f64
        } else if (high_float - low_float).is_finite()
            && (1.0..=32.0).contains(&(high_float - low_float + 1.0))
        {
            high_float - low_float + 1.0
        } else {
            return ENGINE_DEFAULT_SIZE_M;
        };
        extents[axis] = voxels * VOXEL_M;
    }
    extents.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    extents[1]
}

/// Render the non-zero floats in a component field window, e.g. `0x414:12.5`.
#[cfg(feature = "logging")]
/// Every scalar field of a component that could plausibly be a temperature,
/// rate or level. Values in the temperature/rate band (20..200) are grouped
/// separately because that is where the anchors are expected - the preset block
/// read 43.4 C, the player reported 91 C at 49.6 RPS and a 39 C radiator - and
/// an f64 pass covers state stored as a double. Offsets are printed so a field
/// can be identified without knowing it in advance.
#[cfg(feature = "logging")]
unsafe fn scalar_fields(component: usize, limit: usize) -> String {
    let mut hot = String::new();
    let mut rest = String::new();
    let mut doubles = String::new();
    let mut hot_n = 0usize;
    let mut rest_n = 0usize;
    let mut dbl_n = 0usize;
    let mut offset = 0usize;
    let mut chunk_ok = false;
    while offset + 8 <= 0x900 {
        if offset % 64 == 0 {
            chunk_ok = readable(component + offset, 64);
        }
        if !chunk_ok {
            offset += 64;
            continue;
        }
        let as_float = read_f32_at(component + offset);
        let as_int = read_u32_at(component + offset);
        let as_double = read_f64_at(component + offset);
        if as_float.is_finite() && (20.0..=200.0).contains(&as_float) {
            if hot_n < limit {
                hot.push_str(&format!(" {offset:x}:{as_float:.4}"));
                hot_n += 1;
            }
        } else if as_float.is_finite() && as_float.abs() < 0.5 && (1..=200).contains(&as_int) {
            if hot_n < limit {
                hot.push_str(&format!(" {offset:x}:i{as_int}"));
                hot_n += 1;
            }
        } else if as_float.is_finite() && (0.5..=1500.0).contains(&as_float) {
            if rest_n < limit {
                rest.push_str(&format!(" {offset:x}:{as_float:.4}"));
                rest_n += 1;
            }
        }
        if as_double.is_finite() && (20.0..=200.0).contains(&as_double) {
            if dbl_n < limit {
                doubles.push_str(&format!(" {offset:x}:d{as_double:.4}"));
                dbl_n += 1;
            }
        }
        offset += 4;
    }
    format!(" hot:{hot} rest:{rest} dbl:{doubles}")
}

/// Periodic per-part dump for the families whose state is still being
/// identified, deduplicated by definition pointer so one line per part type
/// shows how each candidate field evolves while the engine runs and cools.
#[cfg(feature = "logging")]
unsafe fn dump_class_fields(
    samples: &[crate::state::EngineSample],
    families: &[&str],
    label: &str,
) {
    let mut seen: Vec<usize> = Vec::new();
    let mut printed = 0usize;
    for sample in samples.iter().filter(|sample| families.contains(&sample.family)) {
        // A sample key can outlive its component (despawn), so the
        // readability guard must come before ANY dereference. The previous
        // order read key+0x58 first, which faults on a freed component.
        if !readable(sample.key, 0x40) {
            continue;
        }
        let definition = read_usize_at(sample.key + 0x58);
        if seen.contains(&definition) {
            continue;
        }
        seen.push(definition);
        if printed >= 20 {
            continue;
        }
        printed += 1;
        log_line(&format!(
            "{label} field[{} {definition:#x} {} size={:.2}m vt={:#x} pos=({:.1},{:.1},{:.1})]{}",
            sample.family,
            definition_name(definition),
            sample.size_m,
            read_usize_at(sample.key),
            sample.x,
            sample.y,
            sample.z,
            scalar_fields(sample.key, 60),
        ));
    }
}


unsafe fn float_window(component: usize, base: usize, count: usize) -> String {
    let mut out = String::new();
    for index in 0..count {
        let offset = base + index * 4;
        // Callers verify only the component header, so every read re-verifies
        // its own few bytes rather than trusting the caller's range.
        if !readable(component + offset, 4) {
            continue;
        }
        let value = read_f32_at(component + offset);
        if value != 0.0 && value.is_finite() && value.abs() > 1e-9 {
            out.push_str(&format!(" {offset:x}:{value:.4}"));
        }
    }
    out
}

/// Publish one engine block's IR source (or retract it when the engine stops),
/// keyed by the component so each block has exactly one live source.
unsafe fn publish_engine_source(
    key: usize,
    position: Option<(f64, f64, f64)>,
    power: f64,
    temperature: f64,
    size_m: f64,
) {
    if power <= ENGINE_MIN_POWER || temperature <= 0.0 {
        if let Ok(mut pending) = EMITTER_PENDING.lock() {
            pending.retain(|entry| entry.0 != key);
        }
        return;
    }
    let Some((x, y, z)) = position else {
        return;
    };
    if x == 0.0 && y == 0.0 && z == 0.0 {
        return;
    }
    if let Ok(mut pending) = EMITTER_PENDING.lock() {
        let seen = now_ms();
        if let Some(entry) = pending.iter_mut().find(|entry| entry.0 == key) {
            *entry = (key, seen, x, y, z, power, temperature, size_m);
        } else if pending.len() < EMITTER_MAX_PENDING {
            pending.push((key, seen, x, y, z, power, temperature, size_m));
        }
    }
}

/// Logged before any filtering, so "hook never fired" can be told apart from
/// "hook fired but the component was rejected" - the ambiguity that made the
/// last two test runs useless.
#[cfg(feature = "logging")]
fn note_hook_call(family: &'static str, component: usize) {
    // Shared budget rather than a per-hook counter: the counter only advances
    // for accepted components, so a rejected one logged on every call and
    // produced a 17 MB log.
    static NOTES: AtomicUsize = AtomicUsize::new(0);
    if NOTES.fetch_add(1, Ordering::Relaxed) < 40 {
        log_line(&format!("{family} hook call this={component:#x}"));
    }
}

#[cfg(not(feature = "logging"))]
fn note_hook_call(_family: &'static str, _component: usize) {}

/// Same idea for the rejection diagnostics: a bounded number of lines.
#[cfg(feature = "logging")]
fn note_rejection(message: &str) {
    static NOTES: AtomicUsize = AtomicUsize::new(0);
    if NOTES.fetch_add(1, Ordering::Relaxed) < 20 {
        log_line(message);
    }
}

#[cfg(not(feature = "logging"))]
fn note_rejection(_message: &str) {}

/// Bounded, definition-deduplicated gate for the per-call component probes.
///
/// These were gated on the per-component call count returned by
/// `record_engine_sample`, which holds at most 32 entries: once the table was
/// full, every further component reported "call 1" forever, so the probes
/// logged on every tick - 17 MB in a few minutes, and past 100 MB in a long
/// session, which crashed the game. A probe now fires once per part
/// definition and at most PROBE_DEFS times overall, which is what these probes
/// are actually for: identifying part types, not counting calls.
#[cfg(feature = "logging")]
unsafe fn probe_once(component: usize) -> bool {
    const PROBE_DEFS: usize = 24;
    static SEEN: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
    let definition = read_usize_at(component + 0x58);
    let Ok(mut seen) = SEEN.lock() else {
        return false;
    };
    if seen.contains(&definition) || seen.len() >= PROBE_DEFS {
        return false;
    }
    seen.push(definition);
    true
}

/// A component pointer is only trusted when its vtable points into the image;
/// some call sites pass something else entirely (they showed up as vt=0x0
/// samples with garbage fields).
pub(crate) unsafe fn looks_like_component(component: usize) -> bool {
    if component < 0x10000 {
        return false;
    }
    let vtable = crate::memory::read_usize_at(component);
    // Compare against the module base captured at bootstrap. The first version
    // of this check wrote the image base as 0x1400_0000 (0x14000000, one hex
    // digit short of 0x1_4000_0000) and therefore rejected every real component.
    let base = crate::state::MODULE_BASE.load(Ordering::SeqCst);
    base != 0 && vtable >= base && vtable < base + 0x1000_0000
}

/// Shared probe body for the component classes whose heat state is still being
/// identified (the two diesel engine families and the fluid/heat classes).
/// Nothing is published from here - it records the component (size, vtable and
/// world position from the tick's matrix arguments) and dumps every sane scalar
/// field so the offset carrying load or temperature can be spotted without
/// knowing it in advance.
unsafe fn class_tick_probe(
    component: usize,
    calls: &AtomicUsize,
    family: &'static str,
    matrix: usize,
    origin: usize,
) {
    if !readable(component, 0x40) {
        note_rejection(&format!("{family} component not readable this={component:#x}"));
        return;
    }
    if !looks_like_component(component) {
        note_rejection(&format!(
            "{family} component rejected this={component:#x} vt={:#x}",
            read_usize_at(component)
        ));
        return;
    }
    // Same virtual signature as the jet and rocket ticks, so argument 5 is the
    // world matrix and argument 6 the origin offset. Both are readability
    // checked, so a wrong guess yields None rather than a fault.
    let position = component_world_position(component, matrix, origin).filter(|(x, y, z)| {
        x.is_finite()
            && y.is_finite()
            && z.is_finite()
            && x.abs() < 1.0e6
            && y.abs() < 1.0e6
            && z.abs() < 1.0e6
            && !(*x == 0.0 && *y == 0.0 && *z == 0.0)
    });
    let size_m = component_size_m(component);
    calls.fetch_add(1, Ordering::Relaxed);
    #[cfg(feature = "logging")]
    let _ = record_engine_sample(component, family, 0.0, 0.0, position, size_m);
    #[cfg(not(feature = "logging"))]
    let _ = record_engine_sample(component, family, 0.0, 0.0, position, size_m);
    // The per-definition field dump lives in dump_class_fields (deduplicated
    // and periodic); printing it here too made each first tick ~2 KB.
    #[cfg(feature = "logging")]
    if probe_once(component) {
        let definition = read_usize_at(component + 0x58);
        log_line(&format!(
            "{family} tick[{component:#x}] vt={:#x} def={definition:#x} name={} size={size_m:.2}m \
pos={position:?} matrix={matrix:#x} origin={origin:#x}",
            read_usize_at(component),
            definition_name(definition),
        ));
    }
}


/// Preset (prebuilt) diesel engine block tick. Unlike the jets and rockets the
/// block really does store a temperature, so its IR source uses that value
/// rather than a synthetic curve.
#[allow(clippy::too_many_arguments)]
extern "C" fn ir_preset_tick_hook(
    component: usize,
    p2: usize,
    p3: usize,
    p4: usize,
    p5: usize,
    p6: usize,
    p7: usize,
    p8: usize,
    p9: usize,
    p10: usize,
    p11: usize,
    p12: usize,
    p13: usize,
    p14: usize,
    p15: usize,
    p16: usize,
) -> usize {
    unsafe {
        note_hook_call("preset", component);
        class_tick_probe(component, &PRESET_TICK_CALLS, "preset", p5, p6);
        if readable(component, PRESET_TEMP_OFFSET + 4) && looks_like_component(component) {
            let temperature = read_f32_at(component + PRESET_TEMP_OFFSET) as f64;
            let size_m = component_size_m(component);
            let position = component_world_position(component, p5, p6);
            // A cold block retracts its source; a running one emits at the
            // temperature the game itself reports.
            let power = if temperature >= PRESET_ENGINE_MIN_C {
                1.0
            } else {
                0.0
            };
            publish_engine_source(component, position, power, temperature, size_m);
        }
        let original = ORIG_PRESET_TICK.load(Ordering::SeqCst);
        if original == 0 {
            return 0;
        }
        let f: unsafe extern "C" fn(
            usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize,
            usize, usize, usize, usize,
        ) -> usize = std::mem::transmute(original);
        f(
            component, p2, p3, p4, p5, p6, p7, p8, p9, p10, p11, p12, p13, p14, p15, p16,
        )
    }
}


/// Modular (custom) engine tick, same shape as the preset one.
#[allow(clippy::too_many_arguments)]
extern "C" fn ir_modular_tick_hook(
    component: usize,
    p2: usize,
    p3: usize,
    p4: usize,
    p5: usize,
    p6: usize,
    p7: usize,
    p8: usize,
    p9: usize,
    p10: usize,
    p11: usize,
    p12: usize,
    p13: usize,
    p14: usize,
    p15: usize,
    p16: usize,
) -> usize {
    unsafe {
        note_hook_call("custom", component);
        class_tick_probe(component, &MODULAR_TICK_CALLS, "custom", p5, p6);
        if readable(component, MODULAR_TEMP_OFFSET + 4) && looks_like_component(component) {
            let temperature = read_f32_at(component + MODULAR_TEMP_OFFSET) as f64;
            let size_m = component_size_m(component);
            let position = component_world_position(component, p5, p6);
            // Only cylinder parts carry this field; a cold engine retracts.
            let power = if temperature >= ENGINE_MIN_TEMP_C {
                1.0
            } else {
                0.0
            };
            publish_engine_source(component, position, power, temperature, size_m);
        }
        let original = ORIG_MODULAR_TICK.load(Ordering::SeqCst);
        if original == 0 {
            return 0;
        }
        let f: unsafe extern "C" fn(
            usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize,
            usize, usize, usize, usize,
        ) -> usize = std::mem::transmute(original);
        f(
            component, p2, p3, p4, p5, p6, p7, p8, p9, p10, p11, p12, p13, p14, p15, p16,
        )
    }
}

/// Water-class tick: radiators, heat exchangers, tanks, valves and other
/// fluid machinery all share it, so this probe identifies which part types are
/// in the class and which field holds their temperature.
#[allow(clippy::too_many_arguments)]
extern "C" fn ir_water_tick_hook(
    component: usize,
    p2: usize,
    p3: usize,
    p4: usize,
    p5: usize,
    p6: usize,
    p7: usize,
    p8: usize,
    p9: usize,
    p10: usize,
    p11: usize,
    p12: usize,
    p13: usize,
    p14: usize,
    p15: usize,
    p16: usize,
) -> usize {
    unsafe {
        note_hook_call("fluid", component);
        class_tick_probe(component, &WATER_TICK_CALLS, "fluid", p5, p6);
        if readable(component, FLUID_TEMP_OFFSET + 4) && looks_like_component(component) {
            let temperature = read_f32_at(component + FLUID_TEMP_OFFSET) as f64;
            let size_m = component_size_m(component);
            let position = component_world_position(component, p5, p6);
            let power = if temperature >= ENGINE_MIN_TEMP_C {
                1.0
            } else {
                0.0
            };
            publish_engine_source(component, position, power, temperature, size_m);
        }
        let original = ORIG_WATER_TICK.load(Ordering::SeqCst);
        if original == 0 {
            return 0;
        }
        let f: unsafe extern "C" fn(
            usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize,
            usize, usize, usize, usize,
        ) -> usize = std::mem::transmute(original);
        f(
            component, p2, p3, p4, p5, p6, p7, p8, p9, p10, p11, p12, p13, p14, p15, p16,
        )
    }
}

/// Steam-class tick (boilers, steam turbines, whistles).
#[allow(clippy::too_many_arguments)]
extern "C" fn ir_steam_tick_hook(
    component: usize,
    p2: usize,
    p3: usize,
    p4: usize,
    p5: usize,
    p6: usize,
    p7: usize,
    p8: usize,
    p9: usize,
    p10: usize,
    p11: usize,
    p12: usize,
    p13: usize,
    p14: usize,
    p15: usize,
    p16: usize,
) -> usize {
    unsafe {
        note_hook_call("steam", component);
        class_tick_probe(component, &STEAM_TICK_CALLS, "steam", p5, p6);
        let original = ORIG_STEAM_TICK.load(Ordering::SeqCst);
        if original == 0 {
            return 0;
        }
        let f: unsafe extern "C" fn(
            usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize,
            usize, usize, usize, usize,
        ) -> usize = std::mem::transmute(original);
        f(
            component, p2, p3, p4, p5, p6, p7, p8, p9, p10, p11, p12, p13, p14, p15, p16,
        )
    }
}

/// Rocket-family component tick (solid and liquid motors). Takes sixteen
/// arguments for the same reason as the jet tick.
#[allow(clippy::too_many_arguments)]
extern "C" fn ir_rocket_tick_hook(
    component: usize,
    p2: usize,
    p3: usize,
    p4: usize,
    matrix: usize,
    origin: usize,
    p7: usize,
    p8: usize,
    p9: usize,
    p10: usize,
    p11: usize,
    p12: usize,
    p13: usize,
    p14: usize,
    scene: usize,
    p16: usize,
) {
    unsafe {
        if readable(component, 0x860) && looks_like_component(component) {
            // The tick only runs its burn block when the byte at +0x1d0 is set;
            // otherwise +0x778 keeps its previous value, so an idle rocket must
            // not keep publishing an IR source from a stale reading.
            let burning = read_u32_at(component + 0x1d0) & 0xff != 0;
            let raw_power = read_f32_at(component + 0x778) as f64;
            let position = component_world_position(component, matrix, origin);
            let size_m = component_size_m(component);
            // Solid and liquid motors share one exhaust temperature; only the
            // plume size differs. A motor that consumes fuel but reports no burn
            // fraction (solid boosters) still counts as burning.
            let drain = record_rocket_state(
                component,
                read_f32_at(component + 0x468),
                read_f32_at(component + 0x7a4),
                burning,
            );
            let power = if raw_power > ENGINE_MIN_POWER {
                raw_power
            } else if drain > 0.0 {
                ROCKET_DRAIN_ACTIVITY
            } else {
                0.0
            };
            ROCKET_TICK_CALLS.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "logging")]
            let _ = record_engine_sample(component, "rocket", power, 0.0, position, size_m);
            #[cfg(not(feature = "logging"))]
            let _ = record_engine_sample(component, "rocket", power, 0.0, position, size_m);
            #[cfg(feature = "logging")]
            if probe_once(component) {
                let definition = read_usize_at(component + 0x58);
                let subtype = read_u32_at(definition + 0xed4);
                log_line(&format!(
                    "rocket probe[{component:#x}] power={raw_power:.4} burning={} \
def={definition:#x} sub={subtype} flags={} pos={position:?}",
                    burning as u8,
                    read_u32_at(component + 0x8),
                ));
            }
            if raw_power > 0.0 {
                ROCKET_MAX_POWER_BITS.fetch_max(raw_power.to_bits(), Ordering::Relaxed);
            }
            #[cfg(feature = "logging")]
            {
                use std::sync::atomic::AtomicUsize;
                // First calls overall, plus the first calls that actually show a
                // running engine - otherwise the probes are eaten by the idle
                // frames before the player has switched anything on.
                static ROCKET_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
                static ROCKET_ACTIVE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
                let n = ROCKET_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                let active = power > ENGINE_MIN_POWER;
                let n = if n < 12 {
                    n
                } else if active {
                    100 + ROCKET_ACTIVE_LOG_COUNT.fetch_add(1, Ordering::Relaxed)
                } else {
                    usize::MAX
                };
                if n < 12 || (n >= 100 && n < 112) {
                    log_line(&format!(
                        "rocket tick[{}] component={:#x} power={:.4} thrust=({:.1},{:.1},{:.1}) pos={:?}",
                        n,
                        component,
                        power,
                        read_f32_at(component + 0x848),
                        read_f32_at(component + 0x84c),
                        read_f32_at(component + 0x850),
                        position
                    ));
                }
            }
            let clamped = power.clamp(0.0, 1.0);
            publish_engine_source(
                component,
                position,
                clamped,
                ROCKET_T_MIN_C + (ROCKET_T_MAX_C - ROCKET_T_MIN_C) * clamped,
                size_m,
            );
        }
        let original = ORIG_ROCKET_TICK.load(Ordering::SeqCst);
        if original == 0 {
            return;
        }
        let f: unsafe extern "C" fn(
            usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize,
            usize, usize, usize, usize,
        ) = std::mem::transmute(original);
        f(
            component, p2, p3, p4, matrix, origin, p7, p8, p9, p10, p11, p12, p13, p14, scene, p16,
        );
    }
}

/// Jet-family component tick.
///
/// The tick takes SIXTEEN arguments: Ghidra's `in_stack_00000078` and
/// `in_stack_00000080` are arguments 15 and 16, and the jet tick uses arg 15 as
/// the scene/effects manager (`scene + 0x1178`, `+ 0x680`, `+ 0x1238`). A detour
/// that forwards fewer leaves the missing slots holding its own stack garbage,
/// which the original then dereferences - that crashed the game on the first
/// call after the diagnostics had already logged.
#[allow(clippy::too_many_arguments)]
extern "C" fn ir_jet_tick_hook(
    component: usize,
    p2: usize,
    p3: usize,
    p4: usize,
    matrix: usize,
    origin: usize,
    p7: usize,
    p8: usize,
    p9: usize,
    p10: usize,
    p11: usize,
    p12: usize,
    p13: usize,
    p14: usize,
    scene: usize,
    p16: usize,
) {
    unsafe {
        if readable(component, 0x1dc) && looks_like_component(component) {
            // +0x1bc is the spool speed: every turbine adds to it and the
            // extracted power is proportional to it.
            let power = read_f32_at(component + JET_RPS_OFFSET) as f64;
            let exhaust_c = read_f32_at(component + 0x1cc) as f64;
            let heat = read_f32_at(component + 0x1d8) as f64;
            let field_d0 = read_f32_at(component + 0x1d0) as f64;
            let definition = read_usize_at(component + 0x58);
            let part_type = if definition >= 0x10000 {
                read_u32_at(definition + DEF_JET_PART_TYPE) & 0xff
            } else {
                0
            };
            let position = component_world_position(component, matrix, origin);
            JET_TICK_CALLS.fetch_add(1, Ordering::Relaxed);
            let size_m = component_size_m(component);
            let _ = record_engine_sample(component, "jet", power, heat, position, size_m);
            if power > 0.0 {
                JET_MAX_POWER_BITS.fetch_max(power.to_bits(), Ordering::Relaxed);
            }
            if heat > 0.0 {
                JET_MAX_HEAT_BITS.fetch_max(heat.to_bits(), Ordering::Relaxed);
            }
            #[cfg(feature = "logging")]
            {
                use std::sync::atomic::AtomicUsize;
                static JET_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
                static JET_ACTIVE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
                let n = JET_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                let active = heat > ENGINE_MIN_POWER || power > ENGINE_MIN_POWER;
                let n = if n < 12 {
                    n
                } else if active {
                    100 + JET_ACTIVE_LOG_COUNT.fetch_add(1, Ordering::Relaxed)
                } else {
                    usize::MAX
                };
                if n < 12 || (n >= 100 && n < 112) {
                    log_line(&format!(
                        "jet tick[{}] component={:#x} power={:.4} exhaust_c={:.1} heat={:.4} pos={:?}",
                        n, component, power, exhaust_c, heat, position
                    ));
                }
            }
            // The game has no Celsius gas temperature anywhere: +0x1cc/+0x1d0
            // are enthalpy (fuel * RPS) in two scales, which is how a compressor
            // could read 2382 C at high spool. So the gas path is synthesised
            // per part role. Below JET_RPS_TYPICAL the whole engine scales down
            // toward ambient rather than sitting at running temperatures.
            let ab_on = part_type == JET_EXHAUST_PART_TYPE
                && (read_u32_at(component + JET_AFTERBURNER_OFFSET) & 0xff) != 0;
            let is_exhaust = part_type == JET_EXHAUST_PART_TYPE;
            // The exhaust part never accumulates spool, so its operating point
            // comes from its 0..1 flow field instead of its own RPS.
            let eff_rps = if is_exhaust {
                heat.clamp(0.0, JET_EXHAUST_FLOW_FULL) / JET_EXHAUST_FLOW_FULL * JET_RPS_FULL
            } else {
                power
            };
            let running = if is_exhaust {
                eff_rps > JET_RPS_TYPICAL * 0.01
            } else {
                power > ENGINE_MIN_POWER
            };
            let (t_low, t_high) = if is_exhaust {
                if ab_on {
                    (JET_AB_T_LOW, JET_AB_T_HIGH)
                } else {
                    (JET_EXHAUST_T_LOW, JET_EXHAUST_T_HIGH)
                }
            } else if part_type == 2 || part_type == 3 {
                (JET_CASING_T_LOW, JET_CASING_T_HIGH)
            } else {
                (JET_COMPRESSOR_T_C, JET_COMPRESSOR_T_C)
            };
            let ir_temp_c = if !running {
                0.0
            } else if eff_rps < JET_RPS_TYPICAL {
                t_low * (eff_rps / JET_RPS_TYPICAL)
            } else {
                let blend =
                    ((eff_rps - JET_RPS_TYPICAL) / (JET_RPS_FULL - JET_RPS_TYPICAL)).clamp(0.0, 1.0);
                t_low + (t_high - t_low) * blend
            };
            let activity = if is_exhaust {
                (heat / JET_EXHAUST_FLOW_FULL).clamp(0.0, 1.0)
            } else {
                (power / JET_RPS_FULL).clamp(0.0, 1.0)
            };
            if let Ok(mut parts) = JET_PARTS.lock() {
                // A despawned part never ticks again, so its stamp stopped
                // advancing; drop it here and the heartbeat stops listing jets
                // that no longer exist.
                let seen = now_ms();
                parts.retain(|e| seen.saturating_sub(e.10) < EMITTER_STALE_MS);
                let entry = (
                    component,
                    definition,
                    part_type,
                    if ab_on { 1u32 } else { 0 },
                    power,
                    heat,
                    exhaust_c,
                    field_d0,
                    size_m,
                    ir_temp_c,
                    seen,
                );
                if let Some(slot) = parts.iter_mut().find(|e| e.0 == component) {
                    *slot = entry;
                } else if parts.len() < 48 {
                    parts.push(entry);
                }
            }
            publish_engine_source(component, position, activity, ir_temp_c, size_m);
        }
        let original = ORIG_JET_TICK.load(Ordering::SeqCst);
        if original == 0 {
            return;
        }
        let f: unsafe extern "C" fn(
            usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize,
            usize, usize, usize, usize,
        ) = std::mem::transmute(original);
        f(
            component, p2, p3, p4, matrix, origin, p7, p8, p9, p10, p11, p12, p13, p14, scene, p16,
        );
    }
}

extern "C" fn ir_temperature_hook(
    p1: usize,
    p2: usize,
    p3: usize,
    p4: usize,
    p5: usize,
    p6: usize,
    p7: usize,
    p8: usize,
    p9: usize,
) -> f32 {
    cache_variant(&[p2, p3, p4, p5, p6, p7, p8, p9]);
    TEMP_READY.store(true, Ordering::SeqCst);
    if !TEMP_FIRST.swap(true, Ordering::SeqCst) {
        log_line("temperature hook first call: heat containers cached");
    }

    // Periodic container dump independent of ir.scan: engines and temperature
    // sensors drive this hook even when no seeker component exists, so the
    // diagnostics stay alive in sessions without one.
    #[cfg(feature = "logging")]
    unsafe {
        use std::sync::atomic::AtomicUsize;
        static TEMP_DUMP_COUNT: AtomicUsize = AtomicUsize::new(0);
        let n = TEMP_DUMP_COUNT.fetch_add(1, Ordering::Relaxed);
        if n % 600 == 0 {
            log_line(&format!(
                "temperature hook dump #{n}: heat={:#x} fires={:#x} same={}",
                p2,
                p5,
                p2 == p5
            ));
            log_heat_container(p2);
            log_volume_heat_sources(p2);
            log_fires_container(p5);
            let jet_calls = JET_TICK_CALLS.swap(0, Ordering::Relaxed);
            let rocket_calls = ROCKET_TICK_CALLS.swap(0, Ordering::Relaxed);
            let jet_power = f64::from_bits(JET_MAX_POWER_BITS.swap(0, Ordering::Relaxed));
            let jet_heat = f64::from_bits(JET_MAX_HEAT_BITS.swap(0, Ordering::Relaxed));
            let rocket_power = f64::from_bits(ROCKET_MAX_POWER_BITS.swap(0, Ordering::Relaxed));
            let engine_sources = EMITTER_PENDING
                .lock()
                .map(|pending| pending.len())
                .unwrap_or(0);
            log_line(&format!(
                "engine state: jet_calls={jet_calls} jet_max_power={jet_power:.4} jet_max_heat={jet_heat:.4} \
rocket_calls={rocket_calls} rocket_max_power={rocket_power:.4} ir_sources={engine_sources}"
            ));
            if let Ok(mut samples) = ENGINE_SAMPLES.lock() {
                samples.sort_by(|a, b| {
                    b.max_power
                        .max(b.max_heat)
                        .partial_cmp(&a.max_power.max(a.max_heat))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                log_line(&format!("engine blocks tracked: {}", samples.len()));
                for sample in samples.iter().take(10) {
                    log_line(&format!(
                        "  {} {:#x} name={} size={:.2}m max_power={:.4} max_heat={:.4} burning={} \
drain={:.5} prop={:.4} fuel={:.4} segments={:.1} pos=({:.1},{:.1},{:.1})",
                        sample.family,
                        sample.key,
                        definition_name(read_usize_at(sample.key + 0x58)),
                        sample.size_m,
                        sample.max_power,
                        sample.max_heat,
                        sample.burning as u8,
                        sample.drain,
                        sample.propellant,
                        read_f32_at(sample.key + 0x414),
                        read_f32_at(sample.key + 0x1f0),
                        sample.x,
                        sample.y,
                        sample.z
                    ));
                }
                // Live per-component field windows, so a rocket that only
                // shows state while burning (solid boosters) is captured too.
                for sample in samples.iter().filter(|sample| sample.family == "rocket").take(10) {
                    if !readable(sample.key, 0x790) {
                        continue;
                    }
                    let definition = read_usize_at(sample.key + 0x58);
                    log_line(&format!(
                        "rocket fields[{:#x}] def={:#x} sub={} throttle={:.4} win1c0:{} win270:{} \
win400:{} win760:{}",
                        sample.key,
                        definition,
                        read_u32_at(definition + 0xed4),
                        read_f32_at(sample.key + 0x268),
                        float_window(sample.key, 0x1c0, 8),
                        float_window(sample.key, 0x270, 12),
                        float_window(sample.key, 0x400, 40),
                        float_window(sample.key, 0x760, 20),
                    ));
                }
                dump_class_fields(&samples, &["preset", "custom", "fluid", "steam"], "class");
                samples.clear();
            }
        }
    }

    let original = ORIG_TEMPERATURE.load(Ordering::SeqCst);
    if original == 0 {
        return 0.0;
    }
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
    ) -> f32 = unsafe { std::mem::transmute(original) };
    unsafe { f(p1, p2, p3, p4, p5, p6, p7, p8, p9) }
}

/// Cache the fires-system holder from the fire vertex builder (its arg 1 owns
/// the fires ring at +0x11d8..+0x11e8, the same one the temperature function
/// reads as its fires argument). This makes flares and burning fires visible
/// even before any temperature evaluation (engine/probe) has run this session.
pub(crate) extern "C" fn ir_fires_hook(
    p1: usize,
    p2: usize,
    p3: usize,
    p4: usize,
    p5: usize,
    p6: usize,
) {
    if p1 >= 0x10000 {
        FIRES_HOLDER.store(p1, Ordering::Relaxed);
        static FIRST: AtomicBool = AtomicBool::new(false);
        if !FIRST.swap(true, Ordering::Relaxed) {
            log_line("fires builder hook first call: fires holder cached");
        }
    }
    let original = ORIG_FIRES_BUILD.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    let f: unsafe extern "C" fn(usize, usize, usize, usize, usize, usize) =
        unsafe { std::mem::transmute(original) };
    unsafe { f(p1, p2, p3, p4, p5, p6) }
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
pub(crate) extern "C" fn ir_sun_update_hook(p1: usize, f0: f32, f1: f32, p3: u32, p4: usize) {
    let original = ORIG_SUN_UPDATE.load(Ordering::SeqCst);
    if original != 0 {
        let f: unsafe extern "C" fn(usize, f32, f32, u32, usize) =
            unsafe { std::mem::transmute(original) };
        unsafe { f(p1, f0, f1, p3, p4) };
    }
    if p4 >= 0x10000 {
        let x = unsafe { read_f64_at(p4 + 0x198) };
        let y = unsafe { read_f64_at(p4 + 0x1a0) };
        let z = unsafe { read_f64_at(p4 + 0x1a8) };
        let n = (x * x + y * y + z * z).sqrt();
        if n > 100.0 && n < 1.0e9 {
            if let Ok(mut g) = SUN_DIR.lock() {
                g[0] = x;
                g[1] = y;
                g[2] = z;
            }
            let pct_f = if (0.0..=1.0).contains(&f1) {
                f1
            } else if (0.0..=1.0).contains(&f0) {
                f0
            } else {
                0.0
            };
            SUN_PCT.store(
                (pct_f as f64 * 1000.0).clamp(0.0, 1_000_000.0) as usize,
                Ordering::Relaxed,
            );
            if !SUN_DIR_VALID.swap(true, Ordering::Relaxed) {
                log_line(&format!(
                    "sun update hook first call: sprite=({:.0},{:.0},{:.0}) f0={:.4} f1={:.4}",
                    x, y, z, f0, f1
                ));
            }
        }
    }
}

/// Hook on the per-frame render uniform fill FUN_1406bb400.  It runs on the
/// render thread every frame (unlike the sim-side scene updates, which may
/// only run at world load), and its arg 2 IS the scene object — the very
/// same object whose clock subsystem it touches at +0xbaeb8.  Capturing the
/// scene here makes the day clock and sky state available no matter
/// when the plugin was injected.
///
/// ABI: 15 integer-class arguments.  All are round-tripped unchanged.
#[allow(clippy::too_many_arguments)]
pub(crate) extern "C" fn ir_frame_uniforms_hook(
    p1: usize,
    p2: usize,
    p3: *mut i32,
    p4: *mut f64,
    p5: usize,
    p6: u32,
    p7: i8,
    p8: u8,
    p9: u8,
    p10: usize,
    p11: *mut f64,
    p12: u8,
    p13: u8,
    p14: u8,
    p15: usize,
) {
    let original = ORIG_FRAME_UNIFORMS.load(Ordering::SeqCst);
    if original != 0 {
        let f: unsafe extern "C" fn(
            usize,
            usize,
            *mut i32,
            *mut f64,
            usize,
            u32,
            i8,
            u8,
            u8,
            usize,
            *mut f64,
            u8,
            u8,
            u8,
            usize,
        ) = unsafe { std::mem::transmute(original) };
        unsafe { f(p1, p2, p3, p4, p5, p6, p7, p8, p9, p10, p11, p12, p13, p14, p15) };
    }
    if p2 >= 0x10000 {
        let prev = SCENE.swap(p2, Ordering::Relaxed);
        if prev != p2 {
            static ONCE: AtomicBool = AtomicBool::new(false);
            if !ONCE.swap(true, Ordering::Relaxed) {
                let clock = unsafe { read_u32_at(p2 + 0xbaec0) };
                log_line(&format!(
                    "scene cached from render uniforms: scene={:#x} clock={}",
                    p2, clock
                ));
            }
        }
    }
    if !p11.is_null() && (p11 as usize) >= 0x10000 {
        let base = p11 as usize + 0x8f0; // +0x11e doubles
        let x = unsafe { read_f64_at(base) };
        let y = unsafe { read_f64_at(base + 8) };
        let z = unsafe { read_f64_at(base + 16) };
        let n = (x * x + y * y + z * z).sqrt();
        if n > 1.0e-6 && n < 1.0e9 {
            if let Ok(mut g) = LIGHT_DIR.lock() {
                g[0] = x;
                g[1] = y;
                g[2] = z;
            }
            LIGHT_DIR_VALID.store(true, Ordering::Relaxed);
        }
    }
}

/// Cache the scene object from the scene tick (its arg 1 IS the scene): the
/// day-cycle clock (+0xbaec0, copied from the authoritative +0xbbaac each
/// tick) and the sky state all live on it. Runs every game tick.
pub(crate) extern "C" fn ir_scene_tick_hook(p1: usize, p2: usize, p3: usize) {
    if p1 >= 0x10000 {
        SCENE.store(p1, Ordering::Relaxed);
        static FIRST: AtomicBool = AtomicBool::new(false);
        if !FIRST.swap(true, Ordering::Relaxed) {
            log_line("scene tick hook first call: scene object cached (sun live)");
        }
    }
    let original = ORIG_SCENE_TICK.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    let f: unsafe extern "C" fn(usize, usize, usize) =
        unsafe { std::mem::transmute(original) };
    unsafe { f(p1, p2, p3) }
}

pub(crate) unsafe fn install_hooks() -> Result<(), String> {
    let base = MODULE_BASE.load(Ordering::SeqCst);
    if base == 0 {
        return Err("module base is not resolved".to_string());
    }

    let registrar = base + RVA_COMPONENT_LUA_REGISTRAR;
    if !bytes_match(registrar, &REGISTRAR_PROLOGUE) {
        return Err(format!(
            "component Lua registrar prologue mismatch at {registrar:#x}; this build is not {GAME_BUILD_LABEL}"
        ));
    }
    let temperature = base + RVA_TEMPERATURE;
    if !bytes_match(temperature, &TEMPERATURE_PROLOGUE) {
        return Err(format!(
            "temperature function prologue mismatch at {temperature:#x}; this build is not {GAME_BUILD_LABEL}"
        ));
    }
    let fires_build = base + RVA_FIRES_BUILD;
    if !bytes_match(fires_build, &FIRES_PROLOGUE) {
        return Err(format!(
            "fire vertex builder prologue mismatch at {fires_build:#x}; this build is not {GAME_BUILD_LABEL}"
        ));
    }
    let scene_tick = base + RVA_SCENE_TICK;
    if !bytes_match(scene_tick, &SCENE_TICK_PROLOGUE) {
        return Err(format!(
            "scene tick prologue mismatch at {scene_tick:#x}; this build is not {GAME_BUILD_LABEL}"
        ));
    }
    let sun_update = base + RVA_SUN_UPDATE;
    if !bytes_match(sun_update, &SUN_UPDATE_PROLOGUE) {
        return Err(format!(
            "sun update prologue mismatch at {sun_update:#x}; this build is not {GAME_BUILD_LABEL}"
        ));
    }
    let frame_uniforms = base + RVA_FRAME_UNIFORMS;
    if !bytes_match(frame_uniforms, &FRAME_UNIFORMS_PROLOGUE) {
        return Err(format!(
            "frame uniforms prologue mismatch at {frame_uniforms:#x}; this build is not {GAME_BUILD_LABEL}"
        ));
    }
    // The drawing environment's initializer. Deliberately non-fatal: if this
    // prologue ever mismatches, the seeker and every other hook must still
    // install, and only the on-screen image is lost.
    let screen_init = base + RVA_LUA_SCREEN_INIT;
    if !bytes_match(screen_init, &SCREEN_INIT_PROLOGUE) {
        log_line(&format!(
            "screen Lua init prologue mismatch at {screen_init:#x}; ir.frame will not reach microcontroller scripts"
        ));
    } else if crate::state::SCREEN_HOOKS_DISABLED.load(Ordering::SeqCst) {
        log_line("screen Lua init hook disabled by request");
    } else {
        match MinHook::create_hook(
            screen_init as *mut c_void,
            ir_screen_init_hook as *const c_void as *mut c_void,
        ) {
            Ok(original) => {
                ORIG_SCREEN_INIT.store(original as usize, Ordering::SeqCst);
                if let Err(status) = MinHook::enable_hook(screen_init as *mut c_void) {
                    log_line(&format!("MinHook enable_hook(screen Lua init) failed: {status}"));
                }
            }
            Err(status) => log_line(&format!(
                "MinHook create_hook(screen Lua init) failed: {status}"
            )),
        }
    }

    if crate::state::EXHAUST_HOOKS_DISABLED.load(Ordering::SeqCst) {
        return Ok(());
    }

    let original_registrar = MinHook::create_hook(
        registrar as *mut c_void,
        ir_registrar_hook as *const c_void as *mut c_void,
    )
    .map_err(|status| format!("MinHook create_hook(registrar) failed: {status}"))?;
    ORIG_REGISTRAR.store(original_registrar as usize, Ordering::SeqCst);
    MinHook::enable_hook(registrar as *mut c_void)
        .map_err(|status| format!("MinHook enable_hook(registrar) failed: {status}"))?;

    let original_temperature = MinHook::create_hook(
        temperature as *mut c_void,
        ir_temperature_hook as *const c_void as *mut c_void,
    )
    .map_err(|status| format!("MinHook create_hook(temperature) failed: {status}"))?;
    ORIG_TEMPERATURE.store(original_temperature as usize, Ordering::SeqCst);
    MinHook::enable_hook(temperature as *mut c_void)
        .map_err(|status| format!("MinHook enable_hook(temperature) failed: {status}"))?;

    let original_fires = MinHook::create_hook(
        fires_build as *mut c_void,
        ir_fires_hook as *const c_void as *mut c_void,
    )
    .map_err(|status| format!("MinHook create_hook(fires builder) failed: {status}"))?;
    ORIG_FIRES_BUILD.store(original_fires as usize, Ordering::SeqCst);
    MinHook::enable_hook(fires_build as *mut c_void)
        .map_err(|status| format!("MinHook enable_hook(fires builder) failed: {status}"))?;

    let original_scene_tick = MinHook::create_hook(
        scene_tick as *mut c_void,
        ir_scene_tick_hook as *const c_void as *mut c_void,
    )
    .map_err(|status| format!("MinHook create_hook(scene tick) failed: {status}"))?;
    ORIG_SCENE_TICK.store(original_scene_tick as usize, Ordering::SeqCst);
    MinHook::enable_hook(scene_tick as *mut c_void)
        .map_err(|status| format!("MinHook enable_hook(scene tick) failed: {status}"))?;

    let original_sun_update = MinHook::create_hook(
        sun_update as *mut c_void,
        ir_sun_update_hook as *const c_void as *mut c_void,
    )
    .map_err(|status| format!("MinHook create_hook(sun update) failed: {status}"))?;
    ORIG_SUN_UPDATE.store(original_sun_update as usize, Ordering::SeqCst);
    MinHook::enable_hook(sun_update as *mut c_void)
        .map_err(|status| format!("MinHook enable_hook(sun update) failed: {status}"))?;

    let original_frame_uniforms = MinHook::create_hook(
        frame_uniforms as *mut c_void,
        ir_frame_uniforms_hook as *const c_void as *mut c_void,
    )
    .map_err(|status| format!("MinHook create_hook(frame uniforms) failed: {status}"))?;
    ORIG_FRAME_UNIFORMS.store(original_frame_uniforms as usize, Ordering::SeqCst);
    MinHook::enable_hook(frame_uniforms as *mut c_void)
        .map_err(|status| format!("MinHook enable_hook(frame uniforms) failed: {status}"))?;

    if !ROCKET_HOOKS_DISABLED.load(Ordering::SeqCst) {
        let rocket_tick = base + RVA_ROCKET_TICK;
        if !bytes_match(rocket_tick, &ROCKET_TICK_PROLOGUE) {
            return Err(format!(
                "rocket tick prologue mismatch at {rocket_tick:#x}; this build is not {GAME_BUILD_LABEL}"
            ));
        }
        let original_rocket_tick = MinHook::create_hook(
            rocket_tick as *mut c_void,
            ir_rocket_tick_hook as *const c_void as *mut c_void,
        )
        .map_err(|status| format!("MinHook create_hook(rocket tick) failed: {status}"))?;
        ORIG_ROCKET_TICK.store(original_rocket_tick as usize, Ordering::SeqCst);
        MinHook::enable_hook(rocket_tick as *mut c_void)
            .map_err(|status| format!("MinHook enable_hook(rocket tick) failed: {status}"))?;
    }

    if !JET_HOOKS_DISABLED.load(Ordering::SeqCst) {
        let jet_tick = base + RVA_JET_TICK;
        if !bytes_match(jet_tick, &JET_TICK_PROLOGUE) {
            return Err(format!(
                "jet tick prologue mismatch at {jet_tick:#x}; this build is not {GAME_BUILD_LABEL}"
            ));
        }
        let original_jet_tick = MinHook::create_hook(
            jet_tick as *mut c_void,
            ir_jet_tick_hook as *const c_void as *mut c_void,
        )
        .map_err(|status| format!("MinHook create_hook(jet tick) failed: {status}"))?;
        ORIG_JET_TICK.store(original_jet_tick as usize, Ordering::SeqCst);
        MinHook::enable_hook(jet_tick as *mut c_void)
            .map_err(|status| format!("MinHook enable_hook(jet tick) failed: {status}"))?;
    }

    if !PRESET_HOOKS_DISABLED.load(Ordering::SeqCst) {
        let preset_tick = base + RVA_PRESET_ENGINE_TICK;
        if !bytes_match(preset_tick, &PRESET_ENGINE_TICK_PROLOGUE) {
            return Err(format!(
                "preset engine tick prologue mismatch at {preset_tick:#x}; this build is not {GAME_BUILD_LABEL}"
            ));
        }
        let original_preset_tick = MinHook::create_hook(
            preset_tick as *mut c_void,
            ir_preset_tick_hook as *const c_void as *mut c_void,
        )
        .map_err(|status| format!("MinHook create_hook(preset engine tick) failed: {status}"))?;
        ORIG_PRESET_TICK.store(original_preset_tick as usize, Ordering::SeqCst);
        MinHook::enable_hook(preset_tick as *mut c_void)
            .map_err(|status| format!("MinHook enable_hook(preset engine tick) failed: {status}"))?;
    }

    if !MODULAR_HOOKS_DISABLED.load(Ordering::SeqCst) {
        let modular_tick = base + RVA_MODULAR_ENGINE_TICK;
        if !bytes_match(modular_tick, &MODULAR_ENGINE_TICK_PROLOGUE) {
            return Err(format!(
                "modular engine tick prologue mismatch at {modular_tick:#x}; this build is not {GAME_BUILD_LABEL}"
            ));
        }
        let original_modular_tick = MinHook::create_hook(
            modular_tick as *mut c_void,
            ir_modular_tick_hook as *const c_void as *mut c_void,
        )
        .map_err(|status| format!("MinHook create_hook(modular engine tick) failed: {status}"))?;
        ORIG_MODULAR_TICK.store(original_modular_tick as usize, Ordering::SeqCst);
        MinHook::enable_hook(modular_tick as *mut c_void)
            .map_err(|status| format!("MinHook enable_hook(modular engine tick) failed: {status}"))?;
    }

    if !FLUID_HOOKS_DISABLED.load(Ordering::SeqCst) {
        let water_tick = base + RVA_WATER_TICK;
        if !bytes_match(water_tick, &WATER_TICK_PROLOGUE) {
            return Err(format!(
                "water tick prologue mismatch at {water_tick:#x}; this build is not {GAME_BUILD_LABEL}"
            ));
        }
        let original_water_tick = MinHook::create_hook(
            water_tick as *mut c_void,
            ir_water_tick_hook as *const c_void as *mut c_void,
        )
        .map_err(|status| format!("MinHook create_hook(water tick) failed: {status}"))?;
        ORIG_WATER_TICK.store(original_water_tick as usize, Ordering::SeqCst);
        MinHook::enable_hook(water_tick as *mut c_void)
            .map_err(|status| format!("MinHook enable_hook(water tick) failed: {status}"))?;

        let steam_tick = base + RVA_STEAM_TICK;
        if !bytes_match(steam_tick, &STEAM_TICK_PROLOGUE) {
            return Err(format!(
                "steam tick prologue mismatch at {steam_tick:#x}; this build is not {GAME_BUILD_LABEL}"
            ));
        }
        let original_steam_tick = MinHook::create_hook(
            steam_tick as *mut c_void,
            ir_steam_tick_hook as *const c_void as *mut c_void,
        )
        .map_err(|status| format!("MinHook create_hook(steam tick) failed: {status}"))?;
        ORIG_STEAM_TICK.store(original_steam_tick as usize, Ordering::SeqCst);
        MinHook::enable_hook(steam_tick as *mut c_void)
            .map_err(|status| format!("MinHook enable_hook(steam tick) failed: {status}"))?;
    }

    if !EXHAUST_HOOKS_DISABLED.load(Ordering::SeqCst) {
        let exhaust_update = base + RVA_EXHAUST_UPDATE;
        if !bytes_match(exhaust_update, &EXHAUST_UPDATE_PROLOGUE) {
            return Err(format!(
                "exhaust update prologue mismatch at {exhaust_update:#x}; this build is not {GAME_BUILD_LABEL}"
            ));
        }
        let original_exhaust_update = MinHook::create_hook(
            exhaust_update as *mut c_void,
            ir_exhaust_update_hook as *const c_void as *mut c_void,
        )
        .map_err(|status| format!("MinHook create_hook(exhaust update) failed: {status}"))?;
        ORIG_EXHAUST_UPDATE.store(original_exhaust_update as usize, Ordering::SeqCst);
        MinHook::enable_hook(exhaust_update as *mut c_void)
            .map_err(|status| format!("MinHook enable_hook(exhaust update) failed: {status}"))?;
    }

    HOOKS_INSTALLED.store(true, Ordering::SeqCst);
    Ok(())
}

// ---------------------------------------------------------------------------
// bootstrap
// ---------------------------------------------------------------------------
