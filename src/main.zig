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
//! The game functions we hook (see `hooks`) let the plugin reuse the game's
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
//!   * `offsets`   - game addresses, byte-signature gates, tuning constants.
//!   * `state`     - process-wide runtime state.
//!   * `hooks`     - MinHook detours and the prologue gate.
//!   * `heat`      - the game's heat model (containers, influence, ambient).
//!   * `sources`   - heat/fire/volume-heat/engine-exhaust collectors.
//!   * `ir`        - radiometry, attenuation, projection, occlusion raycast.
//!   * `sun`       - the sun emitter and the day/night gate.
//!   * `api`       - the `ir.probe` / `ir.scan` Lua functions.
//!   * `lua`       - Lua C-API helpers.
//!   * `blocktemp` - per-body block temperature grid (thermal simulation).
//!   * `flir`      - FLIR camera view: block pass, replacement tonemap, F10.
//!   * `memory`    - RVA resolution, prologue checks, primitive reads.
//!   * `logging`   - the single log file.
//!   * `win32`     - the Win32 declarations, mutex and clock.

const std = @import("std");
const fault = @import("fault.zig");
const hooks = @import("hooks.zig");
const logging = @import("logging.zig");
const memory = @import("memory.zig");
const offsets = @import("offsets.zig");
const state = @import("state.zig");
const win = @import("win32.zig");

const verbose = logging.verbose;
const logLine = logging.logLine;
const logf = logging.logf;

/// Every panic is reported on the bootstrap log. Zig cannot unwind out of a
/// detour, so a panic (a failed safety check) ends the process, and the
/// vectored fault handler never sees it: this line is then the only evidence
/// of where we were.
pub const panic = std.debug.FullPanic(panicHandler);

var IN_PANIC: std.atomic.Value(bool) = .init(false);

fn panicHandler(message: []const u8, first_trace_addr: ?usize) noreturn {
    @branchHint(.cold);
    // The handler itself must not recurse if formatting/logging panics.
    if (!IN_PANIC.swap(true, .monotonic)) {
        // One line per panic: a message with newlines stays one record.
        var clean: [512]u8 = undefined;
        const n = @min(message.len, clean.len);
        for (message[0..n], 0..) |c, i| clean[i] = if (c == '\n' or c == '\r') ' ' else c;
        const stage = fault.STAGE.load(.monotonic);
        // `logLine` opens, writes and closes once, so this survives the
        // abort that follows.
        logf("PANIC: {s} at 0x{x} thread={d} stage={s} ({d})", .{
            clean[0..n],
            first_trace_addr orelse @returnAddress(),
            win.GetCurrentThreadId(),
            fault.stageName(stage),
            stage,
        });
    }
    win.ExitProcess(3);
}

fn heartbeat(log_dir: win.WidePath) void {
    const path = log_dir.join("infrared_heartbeat.log");
    // Re-initialise per injection so old sessions do not mix in - the
    // per-beat writes below append by design, this only clears the file this
    // session starts.
    const handle = win.CreateFileW(path.z(), win.GENERIC_WRITE, win.FILE_SHARE_READ | win.FILE_SHARE_WRITE, null, win.CREATE_ALWAYS, win.FILE_ATTRIBUTE_NORMAL, null);
    if (logging.validHandle(handle)) _ = win.CloseHandle(handle);
    var beat: u64 = 0;
    var text: std.ArrayList(u8) = .empty;
    defer text.deinit(state.gpa);
    while (true) {
        win.Sleep(10_000);
        beat += 1;
        text.clearRetainingCapacity();
        writeBeat(&text, beat) catch continue;
        const file = logging.openAppend(path.z()) orelse continue;
        logging.writeAll(file, text.items);
        _ = win.CloseHandle(file);
    }
}

fn writeBeat(text: *std.ArrayList(u8), beat: u64) !void {
    const gpa = state.gpa;
    const fire_i: f32 = @bitCast(state.FIRE_INTENSITY_MAX_BITS.load(.monotonic));
    const fire_only: f32 = @bitCast(state.FIRE_ONLY_MAX_BITS.load(.monotonic));
    const smoke_only: f32 = @bitCast(state.SMOKE_ONLY_MAX_BITS.load(.monotonic));
    const samples = blk: {
        const s = state.ENGINE_SAMPLES.lock();
        defer state.ENGINE_SAMPLES.unlock();
        break :blk s.items.len;
    };
    // In-cone sources from the last scan. Player-build diagnostics:
    // distance, effective radius and flux identify an emitter and let its
    // temperature be back-solved offline.
    var sources: std.ArrayList(state.SourceSnapshot) = .empty;
    defer sources.deinit(gpa);
    {
        const s = state.LAST_SOURCES.lock();
        defer state.LAST_SOURCES.unlock();
        try sources.appendSlice(gpa, s.items);
    }
    // Per-jet-part raw fields: power/heat/exhaust are the three candidates
    // for what a jet's IR should actually be driven by.
    var jets: std.ArrayList(state.JetPart) = .empty;
    defer jets.deinit(gpa);
    {
        const j = state.JET_PARTS.lock();
        defer state.JET_PARTS.unlock();
        // Entries from despawned components have a stamp that stopped
        // advancing; their tick is gone, so drop them here.
        const now = state.nowMs();
        for (j.items) |entry| {
            if (now -| entry.seen_ms < state.EMITTER_STALE_MS) try jets.append(gpa, entry);
        }
    }
    try text.print(gpa, "heartbeat {d}: hooks={d} calls custom={d} preset={d} fluid={d} steam={d} jet={d} rocket={d} screen={d} samples={d} fire_i={d:.6} fire_only={d:.6} smoke_only={d:.6} srcs={d} jets={d}\n", .{
        beat,
        @intFromBool(state.HOOKS_INSTALLED.load(.monotonic)),
        state.MODULAR_TICK_CALLS.load(.monotonic),
        state.PRESET_TICK_CALLS.load(.monotonic),
        state.WATER_TICK_CALLS.load(.monotonic),
        state.STEAM_TICK_CALLS.load(.monotonic),
        state.JET_TICK_CALLS.load(.monotonic),
        state.ROCKET_TICK_CALLS.load(.monotonic),
        state.SCREEN_INIT_HITS.load(.monotonic),
        samples,
        fire_i,
        fire_only,
        smoke_only,
        sources.items.len,
        jets.items.len,
    });
    for (jets.items) |j| {
        try text.print(gpa, "  jet def=0x{x} type={d} ab={d} rps={d:.2} f1d8={d:.5} f1cc={d:.5} f1d0={d:.5} size={d:.2} m -> ir_temp={d:.1} C\n", .{
            j.definition, j.part_type, j.afterburner, j.rps, j.heat, j.exhaust, j.d0, j.size_m, j.ir_temp_c,
        });
    }
    for (sources.items, 0..) |s, i| {
        try text.print(gpa, "  src[{d}]{s}: dist={d:.1} m r_eff={d:.3} m flux={e:.6} temp={d:.1} C\n", .{
            i, if (s.sky) " SKY" else "", s.dist_m, s.r_eff, s.flux, s.temp_c,
        });
    }
}

fn heartbeatThread(param: ?*anyopaque) callconv(.winapi) u32 {
    const dir: *win.WidePath = @ptrCast(@alignCast(param));
    heartbeat(dir.*);
    return 0;
}

/// A kill switch: the env var is "1" or the marker file exists next to the DLL.
fn switchOn(plugin_dir: win.WidePath, comptime env: ?[]const u8, marker: []const u8) bool {
    if (env) |name| if (win.envIsOne(name)) return true;
    return plugin_dir.join(marker).exists();
}

fn bootstrap() void {
    const dll_path = memory.selfModulePath() orelse return;
    const plugin_dir = dll_path.parent();
    state.PLUGIN_DIR = plugin_dir;
    const log_dir = plugin_dir.join("logs");
    _ = win.CreateDirectoryW(log_dir.z(), null);
    const log_path = log_dir.join("infrared_bootstrap.log");
    if (log_path.exists()) {
        // Keep the previous session: the log is truncated on every bootstrap,
        // which silently destroyed a test run when the game was restarted
        // before the log had been read.
        const prev = log_dir.join("infrared_bootstrap.prev.log");
        _ = win.MoveFileExW(log_path.z(), prev.z(), win.MOVEFILE_REPLACE_EXISTING);
    }
    logging.setLogPath(log_path);
    // Verbose builds: write one line per distinct fault address (access
    // violation / illegal instruction / stack overflow) and keep the stage
    // breadcrumb that line reports.
    fault.install(&log_path);

    var path_utf8: [1024]u8 = undefined;
    logf("infrared bootstrap start build={s} plugin={s} dll={s}", .{ offsets.GAME_BUILD_LABEL, offsets.PLUGIN_BUILD_LABEL, dll_path.utf8(&path_utf8) });

    // Liveness probe. Verbose builds only: the player build stays silent, so
    // the only log it writes is the bootstrap. The bootstrap log advances
    // whenever the game calls a hooked function, so "loaded but never called"
    // and "dead" stay distinguishable there.
    if (verbose) {
        const dir = state.gpa.create(win.WidePath) catch null;
        if (dir) |d| {
            d.* = log_dir;
            const thread = win.CreateThread(null, 0, &heartbeatThread, d, 0, null);
            if (thread != null) _ = win.CloseHandle(thread) else state.gpa.destroy(d);
        }
    }

    const base = @intFromPtr(win.GetModuleHandleW(null));
    if (base == 0) {
        logLine("could not resolve the main module base");
        return;
    }
    state.MODULE_BASE.store(base, .seq_cst);
    logf("game module base = 0x{x}", .{base});
    // Kill switch: lets a user bisect a suspected crash without a rebuild.
    // Either create `<game>/infrared_disable_exhaust` next to the log or set
    // INFRARED_NO_EXHAUST=1 before launching the game.
    if (switchOn(plugin_dir, "INFRARED_NO_EXHAUST", "infrared_disable_exhaust")) {
        state.EXHAUST_HOOKS_DISABLED.store(true, .seq_cst);
        logLine("exhaust hooks disabled by request (INFRARED_NO_EXHAUST / marker file)");
    }
    // Deep field dumps walk unchecked game pointers (two crashes traced there): opt-in only.
    if (switchOn(plugin_dir, "INFRARED_DEEPDUMP", "infrared_enable_deepdump")) {
        state.DEEP_DUMP_DISABLED.store(false, .seq_cst);
        logLine("deep field dumps enabled by request (INFRARED_DEEPDUMP / marker file)");
    }
    if (switchOn(plugin_dir, "INFRARED_NO_TICKS", "infrared_disable_rocket")) {
        state.ROCKET_HOOKS_DISABLED.store(true, .seq_cst);
        logLine("rocket tick hook disabled by request");
    }
    if (switchOn(plugin_dir, "INFRARED_NO_TICKS", "infrared_disable_jet")) {
        state.JET_HOOKS_DISABLED.store(true, .seq_cst);
        logLine("jet tick hook disabled by request");
    }

    if (switchOn(plugin_dir, "INFRARED_NO_ENGINES", "infrared_disable_preset")) {
        state.PRESET_HOOKS_DISABLED.store(true, .seq_cst);
        logLine("preset engine tick hook disabled by request");
    }
    if (switchOn(plugin_dir, "INFRARED_NO_ENGINES", "infrared_disable_modular")) {
        state.MODULAR_HOOKS_DISABLED.store(true, .seq_cst);
        logLine("modular engine tick hook disabled by request");
    }

    if (switchOn(plugin_dir, "INFRARED_NO_FLUID", "infrared_disable_fluid")) {
        state.FLUID_HOOKS_DISABLED.store(true, .seq_cst);
        logLine("fluid (water/steam) tick hooks disabled by request");
    }
    if (switchOn(plugin_dir, "INFRARED_NO_SCREEN", "infrared_disable_screen")) {
        state.SCREEN_HOOKS_DISABLED.store(true, .seq_cst);
        logLine("screen Lua init hook disabled by request");
    }
    if (switchOn(plugin_dir, "INFRARED_NO_FLIR", "infrared_disable_flir")) {
        state.FLIR_DISABLED.store(true, .seq_cst);
        logLine("FLIR camera-view hooks disabled by request");
    }

    // Per-subsystem switches for the FLIR path (marker files next to the DLL,
    // like the switches above): bisect a crash without a rebuild.
    const blockpass_disabled = switchOn(plugin_dir, null, "infrared_disable_blockpass");
    if (blockpass_disabled) {
        state.BLOCKPASS_DISABLED.store(true, .seq_cst);
        logLine("FLIR block substitution disabled by request (infrared_disable_blockpass)");
    }
    const sim_disabled = switchOn(plugin_dir, null, "infrared_disable_sim");
    if (sim_disabled) {
        state.SIM_DISABLED.store(true, .seq_cst);
        logLine("block temperature sim disabled by request (infrared_disable_sim)");
    }
    const composite_disabled = switchOn(plugin_dir, null, "infrared_disable_composite");
    if (composite_disabled) {
        state.COMPOSITE_DISABLED.store(true, .seq_cst);
        logLine("FLIR composite disabled by request (infrared_disable_composite)");
    }
    logf("kill switches active: blockpass={} sim={} composite={} flir={}", .{
        blockpass_disabled, sim_disabled, composite_disabled, state.FLIR_DISABLED.load(.seq_cst),
    });

    var install_error: hooks.InstallError = .{};
    hooks.installHooks(&install_error) catch {
        logf("hook installation failed: {s}", .{install_error.message()});
        return;
    };
    logLine("hooks installed (registrar, screen Lua init, temperature, fires, scene, sun, frame uniforms, rocket/jet/preset/modular/water/steam ticks, exhaust)");

    // Keep the worker alive so the log stays writable and the DLL is not
    // considered idle; also report registrar activity periodically.
    for (0..600) |_| {
        win.Sleep(1000);
        const hits = state.REGISTRAR_HITS.load(.monotonic);
        if (hits > 0) {
            logf("ir table registered {d} time(s)", .{hits});
            break;
        }
    }
}

fn bootstrapThread(_: ?*anyopaque) callconv(.winapi) u32 {
    win.Sleep(500);
    bootstrap();
    return 0;
}

pub export fn DllMain(module: win.HMODULE, reason: u32, reserved: ?*anyopaque) callconv(.winapi) i32 {
    const DLL_PROCESS_ATTACH: u32 = 1;
    const DLL_PROCESS_DETACH: u32 = 0;
    if (reason == DLL_PROCESS_DETACH) fault.noteDetach(reserved != null);
    if (reason == DLL_PROCESS_ATTACH) {
        _ = win.DisableThreadLibraryCalls(module);
        state.SELF_MODULE.store(@intFromPtr(module), .seq_cst);
        // If the thread cannot be created there is nothing useful to do: the
        // DLL stays loaded and silently does nothing.
        const thread = win.CreateThread(null, 0, &bootstrapThread, null, 0, null);
        if (thread != null) _ = win.CloseHandle(thread);
    }
    return 1;
}
