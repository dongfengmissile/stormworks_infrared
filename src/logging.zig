//! The single log file (`logs/infrared_bootstrap.log`), truncated once per
//! game launch. Everything else logs through `logLine` / `logf`.

const std = @import("std");
const build_options = @import("build_options");
const win = @import("win32.zig");
const state = @import("state.zig");

/// Verbose diagnostics are compiled in (debug-style builds, see build.zig).
pub const verbose = build_options.logging;

const Atomic = std.atomic.Value;

/// Bootstrap stamp (ms on the plugin clock) for the verbose log's per-line
/// millisecond prefix.
var start_ms: Atomic(u64) = .init(0);

fn elapsedMs() u64 {
    return win.elapsedMs() -| start_ms.load(.monotonic);
}

/// Hard cap on the verbose log. Every probe is individually budgeted, but a
/// runaway line source must not be able to fill the disk or stall the game: a
/// long session once passed 100 MB and crashed it. Past this the log stops,
/// with one final marker line so a truncated log is never mistaken for a
/// crashed plugin.
const LOG_BUDGET_BYTES: usize = 8 * 1024 * 1024;
var LOG_BYTES: Atomic(usize) = .init(0);
var LOG_CAPPED: Atomic(bool) = .init(false);

var LOG_PATH: win.Mutex(?win.WidePath) = .{ .value = null };

pub fn setLogPath(path: win.WidePath) void {
    // The verbose log's millisecond clock starts when the bootstrap log is set
    // up, so its first line is 0 ms.
    if (verbose) start_ms.store(win.elapsedMs(), .monotonic);
    // Re-initialise the log every launch so old sessions do not mix in.
    const handle = win.CreateFileW(path.z(), win.GENERIC_WRITE, win.FILE_SHARE_READ | win.FILE_SHARE_WRITE, null, win.CREATE_ALWAYS, win.FILE_ATTRIBUTE_NORMAL, null);
    if (validHandle(handle)) _ = win.CloseHandle(handle);
    LOG_BYTES.store(0, .monotonic);
    LOG_CAPPED.store(false, .monotonic);
    const slot = LOG_PATH.lock();
    defer LOG_PATH.unlock();
    slot.* = path;
}

pub fn validHandle(handle: win.HANDLE) bool {
    return handle != null and @intFromPtr(handle) != win.INVALID_HANDLE_VALUE;
}

/// Open `path` for appending (created if missing).
pub fn openAppend(path: [*:0]const u16) win.HANDLE {
    const handle = win.CreateFileW(path, win.FILE_APPEND_DATA, win.FILE_SHARE_READ | win.FILE_SHARE_WRITE | win.FILE_SHARE_DELETE, null, win.OPEN_ALWAYS, win.FILE_ATTRIBUTE_NORMAL, null);
    return if (validHandle(handle)) handle else null;
}

pub fn writeAll(handle: win.HANDLE, bytes: []const u8) void {
    var rest = bytes;
    while (rest.len > 0) {
        var written: u32 = 0;
        const chunk: u32 = @intCast(@min(rest.len, 1 << 30));
        if (win.WriteFile(handle, rest.ptr, chunk, &written, null) == 0 or written == 0) return;
        rest = rest[written..];
    }
}

pub fn logLine(message: []const u8) void {
    const slot = LOG_PATH.lock();
    defer LOG_PATH.unlock();
    const path = &(slot.* orelse return);
    if (LOG_BYTES.load(.monotonic) >= LOG_BUDGET_BYTES) {
        if (!LOG_CAPPED.swap(true, .monotonic)) {
            if (openAppend(path.z())) |file| {
                defer _ = win.CloseHandle(file);
                var buf: [256]u8 = undefined;
                const text = (if (verbose)
                    std.fmt.bufPrint(&buf, "[{d} ms] log budget of {d} MB reached; further logging is disabled for this launch. This is the verbose build - the player build logs almost nothing.\n", .{ elapsedMs(), LOG_BUDGET_BYTES / (1024 * 1024) })
                else
                    std.fmt.bufPrint(&buf, "log budget of {d} MB reached; further logging is disabled for this launch. This is the verbose build - the player build logs almost nothing.\n", .{LOG_BUDGET_BYTES / (1024 * 1024)})) catch return;
                writeAll(file, text);
            }
        }
        return;
    }
    // One open + one write + one close per line with no buffering in between,
    // so the line has already reached the OS by the time this returns: the
    // last line before a crash is on disk.
    const file = openAppend(path.z()) orelse return;
    defer _ = win.CloseHandle(file);
    // Every verbose line carries the time since bootstrap. One place, so no
    // call site can forget it (or drift on its own clock).
    if (verbose) {
        var prefix: [32]u8 = undefined;
        writeAll(file, std.fmt.bufPrint(&prefix, "[{d} ms] ", .{elapsedMs()}) catch "");
    }
    writeAll(file, message);
    writeAll(file, "\n");
    _ = LOG_BYTES.fetchAdd(message.len + 24, .monotonic);
}

/// Format and log one line. Short lines format on the stack; a line longer
/// than that is formatted on the heap (and dropped if even that fails).
pub fn logf(comptime fmt: []const u8, args: anytype) void {
    var buf: [2048]u8 = undefined;
    if (std.fmt.bufPrint(&buf, fmt, args)) |text| {
        logLine(text);
    } else |_| {
        const text = std.fmt.allocPrint(state.gpa, fmt, args) catch return;
        defer state.gpa.free(text);
        logLine(text);
    }
}

// ---------------------------------------------------------------------------
// One-shot diagnostics (verbose builds only)
// ---------------------------------------------------------------------------

var LOG_ONCE_BITS: Atomic(u32) = .init(0);

/// Emit the message the first time `key` is used this launch.
///
/// Keys are bits of one process-wide word and every call site owns its own bit
/// (`blocktemp` owns 0..8, `flir` 8..22, `hooks` 22..32): two messages sharing
/// a key means the second one is silently never logged. Compiled out of the
/// player build, like every other verbose diagnostic.
pub fn logOnce(key: u32, comptime fmt: []const u8, args: anytype) void {
    if (!verbose) return;
    // Plain load first: callers sit on per-draw paths, and an RMW on every
    // call would bounce the cache line.
    if (LOG_ONCE_BITS.load(.monotonic) & key == 0 and LOG_ONCE_BITS.fetchOr(key, .monotonic) & key == 0) {
        logf(fmt, args);
    }
}

/// `logf`, compiled out of the player build.
pub inline fn vlogf(comptime fmt: []const u8, args: anytype) void {
    if (verbose) logf(fmt, args);
}
