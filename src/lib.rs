//! Stormworks `infrared` plugin.
//!
//! Injects an `ir` global table into the component-mod Lua state and exposes
//! the game's own heat / temperature evaluator to component scripts:
//!
//! ```lua
//! ir.probe(x, y, z)                                  -> temperature (Celsius)
//! ir.scan(az, el, fov, range, rx, ry, vmin, vmax, axis)
//!     -> cells[1..rx*ry], az_err, el_err, strength
//! ```
//!
//! The game functions we hook (see [`hooks`]) let the plugin reuse the game's
//! own heat sources, fires, sun and weather instead of approximating them:
//!   * the component-mod Lua API registrar, so we can append the `ir` table
//!     using the game's own `register_table` helper;
//!   * the temperature-at-a-world-point function, so we can capture the
//!     heat-container arguments the game passes in and then evaluate them for
//!     arbitrary sample points;
//!   * the fire vertex builder, the scene tick, the sun update, the render
//!     uniform fill and the audio-emitter update, for the fires ring, the scene
//!     pointer, the live sun direction and the engine exhaust emitters.
//!
//! Since the temperature function already applies the component heat sources,
//! fires, lava and base ambient temperature, the scan sees exactly the same
//! heat the stock Temperature Probe does.
//!
//! Layout:
//!   * [`offsets`] - game addresses, byte-signature gates, tuning constants.
//!   * [`state`]   - process-wide runtime state.
//!   * [`hooks`]   - MinHook detours and the prologue gate.
//!   * [`heat`]    - the game's heat model (containers, influence, ambient).
//!   * [`sources`] - heat/fire/volume-heat/engine-exhaust collectors.
//!   * [`ir`]      - radiometry, attenuation, projection, occlusion raycast.
//!   * [`sun`]     - the sun emitter and the day/night gate.
//!   * [`api`]     - the `ir.probe` / `ir.scan` Lua functions.
//!   * [`lua`]     - Lua C-API helpers.
//!   * [`memory`]  - RVA resolution, prologue checks, primitive reads.
//!   * [`logging`] - the single log file.

#![cfg(windows)]
#![allow(non_snake_case, non_upper_case_globals, dead_code)]

mod api;
mod heat;
mod hooks;
mod ir;
mod logging;
mod lua;
mod memory;
mod offsets;
mod sources;
mod state;
mod sun;

use std::ffi::c_void;
use std::fs;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::HINSTANCE;
use windows_sys::Win32::System::LibraryLoader::{DisableThreadLibraryCalls, GetModuleHandleW};

use crate::hooks::install_hooks;
use crate::logging::{log_line, set_log_path};
use std::io::Write;
use crate::memory::self_module_path;
use crate::offsets::GAME_BUILD_LABEL;
use crate::state::{MODULE_BASE, REGISTRAR_HITS, SELF_MODULE};

fn bootstrap() {
    let Some(dll_path) = self_module_path() else {
        return;
    };
    let plugin_dir = dll_path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let log_dir = plugin_dir.join("logs");
    let _ = fs::create_dir_all(&log_dir);
    let log_path = log_dir.join("infrared_bootstrap.log");
    if log_path.exists() {
        // Keep the previous session: the log is truncated on every bootstrap,
        // which silently destroyed a test run when the game was restarted
        // before the log had been read.
        let _ = fs::rename(&log_path, log_dir.join("infrared_bootstrap.prev.log"));
    }
    set_log_path(log_path);

    log_line(&format!(
        "infrared bootstrap start build={GAME_BUILD_LABEL} dll={}",
        dll_path.display()
    ));

    // Liveness probe. Verbose builds only: the player build stays silent, so
    // the only log it writes is the bootstrap. The bootstrap log advances
    // whenever the game calls a hooked function, so "loaded but never called"
    // and "dead" stay distinguishable there.
    #[cfg(feature = "logging")]
    {
        let dir = log_dir.clone();
        let _ = std::thread::spawn(move || {
            let path = dir.join("infrared_heartbeat.log");
            // Re-initialise per injection so old sessions do not mix in - the
            // per-beat writes below append by design, this only clears the
            // file this session starts.
            let _ = fs::write(&path, b"");
            let mut beat = 0u64;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(10));
                beat += 1;
                let custom = crate::state::MODULAR_TICK_CALLS.load(Ordering::Relaxed);
                let preset = crate::state::PRESET_TICK_CALLS.load(Ordering::Relaxed);
                let fluid = crate::state::WATER_TICK_CALLS.load(Ordering::Relaxed);
                let steam = crate::state::STEAM_TICK_CALLS.load(Ordering::Relaxed);
                let jet = crate::state::JET_TICK_CALLS.load(Ordering::Relaxed);
                let rocket = crate::state::ROCKET_TICK_CALLS.load(Ordering::Relaxed);
                let screen = crate::state::SCREEN_INIT_HITS.load(Ordering::Relaxed);
                let fire_i = f32::from_bits(
                    crate::state::FIRE_INTENSITY_MAX_BITS.load(Ordering::Relaxed),
                );
                let fire_only = f32::from_bits(
                    crate::state::FIRE_ONLY_MAX_BITS.load(Ordering::Relaxed),
                );
                let smoke_only = f32::from_bits(
                    crate::state::SMOKE_ONLY_MAX_BITS.load(Ordering::Relaxed),
                );
                let samples = crate::state::ENGINE_SAMPLES
                    .lock()
                    .map(|s| s.len())
                    .unwrap_or(0);
                // In-cone sources from the last scan. Player-build diagnostics:
                // distance, effective radius and flux identify an emitter and
                // let its temperature be back-solved offline.
                let sources = crate::state::LAST_SOURCES
                    .lock()
                    .map(|s| s.clone())
                    .unwrap_or_default();
                // Per-jet-part raw fields: power/heat/exhaust are the three
                // candidates for what a jet's IR should actually be driven by.
                let mut jets = crate::state::JET_PARTS
                    .lock()
                    .map(|j| j.clone())
                    .unwrap_or_default();
                // Entries from despawned components have a stamp that stopped
                // advancing; their tick is gone, so drop them here.
                let now = crate::state::now_ms();
                jets.retain(|entry| now.saturating_sub(entry.10) < crate::state::EMITTER_STALE_MS);
                if let Ok(mut file) = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    let _ = writeln!(
                        file,
                        "heartbeat {beat}: hooks={} calls custom={custom} preset={preset} fluid={fluid} \
steam={steam} jet={jet} rocket={rocket} screen={screen} samples={samples} \
fire_i={fire_i:.6} fire_only={fire_only:.6} smoke_only={smoke_only:.6} srcs={} jets={}",
                        crate::state::HOOKS_INSTALLED.load(Ordering::Relaxed) as u8,
                        sources.len(),
                        jets.len()
                    );
                    for (
                        _comp,
                        def,
                        part,
                        ab,
                        power,
                        heat,
                        exhaust,
                        d0,
                        size_m,
                        ir_temp,
                        _seen,
                    ) in jets.iter()
                    {
                        let _ = writeln!(
                            file,
                            "  jet def={def:#x} type={part} ab={ab} rps={power:.2} f1d8={heat:.5} \
f1cc={exhaust:.5} f1d0={d0:.5} size={size_m:.2} m -> ir_temp={ir_temp:.1} C"
                        );
                    }
                    for (i, (dist, r_eff, flux, sky, temp_c)) in sources.iter().enumerate() {
                        let _ = writeln!(
                            file,
                            "  src[{i}]{}: dist={dist:.1} m r_eff={r_eff:.3} m flux={flux:.6e} \
temp={temp_c:.1} C",
                            if *sky { " SKY" } else { "" }
                        );
                    }
                }
            }
        });
    }

    let base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
    if base == 0 {
        log_line("could not resolve the main module base");
        return;
    }
    MODULE_BASE.store(base, Ordering::SeqCst);
    log_line(&format!("game module base = {base:#x}"));

    // Kill switch: lets a user bisect a suspected crash without a rebuild.
    // Either create `<game>/infrared_disable_exhaust` next to the log or set
    // INFRARED_NO_EXHAUST=1 before launching the game.
    let exhaust_disabled = std::env::var("INFRARED_NO_EXHAUST").as_deref() == Ok("1")
        || plugin_dir.join("infrared_disable_exhaust").exists();
    if exhaust_disabled {
        crate::state::EXHAUST_HOOKS_DISABLED.store(true, Ordering::SeqCst);
        log_line("exhaust hooks disabled by request (INFRARED_NO_EXHAUST / marker file)");
    }
    if std::env::var("INFRARED_NO_TICKS").as_deref() == Ok("1")
        || plugin_dir.join("infrared_disable_rocket").exists()
    {
        crate::state::ROCKET_HOOKS_DISABLED.store(true, Ordering::SeqCst);
        log_line("rocket tick hook disabled by request");
    }
    if std::env::var("INFRARED_NO_TICKS").as_deref() == Ok("1")
        || plugin_dir.join("infrared_disable_jet").exists()
    {
        crate::state::JET_HOOKS_DISABLED.store(true, Ordering::SeqCst);
        log_line("jet tick hook disabled by request");
    }

    if std::env::var("INFRARED_NO_ENGINES").as_deref() == Ok("1")
        || plugin_dir.join("infrared_disable_preset").exists()
    {
        crate::state::PRESET_HOOKS_DISABLED.store(true, Ordering::SeqCst);
        log_line("preset engine tick hook disabled by request");
    }
    if std::env::var("INFRARED_NO_ENGINES").as_deref() == Ok("1")
        || plugin_dir.join("infrared_disable_modular").exists()
    {
        crate::state::MODULAR_HOOKS_DISABLED.store(true, Ordering::SeqCst);
        log_line("modular engine tick hook disabled by request");
    }

    if std::env::var("INFRARED_NO_FLUID").as_deref() == Ok("1")
        || plugin_dir.join("infrared_disable_fluid").exists()
    {
        crate::state::FLUID_HOOKS_DISABLED.store(true, Ordering::SeqCst);
        log_line("fluid (water/steam) tick hooks disabled by request");
    }
    if std::env::var("INFRARED_NO_SCREEN").as_deref() == Ok("1")
        || plugin_dir.join("infrared_disable_screen").exists()
    {
        crate::state::SCREEN_HOOKS_DISABLED.store(true, Ordering::SeqCst);
        log_line("screen Lua init hook disabled by request");
    }

    match unsafe { install_hooks() } {
        Ok(()) => log_line(
            "hooks installed (registrar, screen Lua init, temperature, fires, scene, sun, frame \
uniforms, rocket/jet/preset/modular/water/steam ticks, exhaust)",
        ),
        Err(error) => {
            log_line(&format!("hook installation failed: {error}"));
            return;
        }
    }

    // Keep the worker alive so the log stays writable and the DLL is not
    // considered idle; also report registrar activity periodically.
    for _ in 0..600 {
        thread::sleep(Duration::from_secs(1));
        if REGISTRAR_HITS.load(Ordering::Relaxed) > 0 {
            log_line(&format!(
                "ir table registered {} time(s)",
                REGISTRAR_HITS.load(Ordering::Relaxed)
            ));
            break;
        }
    }
}

#[no_mangle]
pub extern "system" fn DllMain(module: HINSTANCE, reason: u32, _reserved: *mut c_void) -> i32 {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason == DLL_PROCESS_ATTACH {
        unsafe {
            let _ = DisableThreadLibraryCalls(module);
        }
        SELF_MODULE.store(module as usize, Ordering::SeqCst);
        let spawned = thread::Builder::new()
            .name("stormworks-infrared-bootstrap".to_string())
            .spawn(|| {
                thread::sleep(Duration::from_millis(500));
                bootstrap();
            });
        if spawned.is_err() {
            // Nothing useful to do here; the DLL stays loaded and silently
            // does nothing.
        }
    }
    1
}
