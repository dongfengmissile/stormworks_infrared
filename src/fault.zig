//! Crash diagnostics for the verbose build.
//!
//! Two pieces, both nearly free:
//!
//!   * `set` - a stage breadcrumb. Every phase that can still be running when
//!     the game faults stores one atomic u32, so a fault line says where we
//!     were (which phase of the block sim, the mesh map, a FLIR draw, ...).
//!   * `install` - a vectored exception handler (last in the chain, i.e.
//!     `AddVectoredExceptionHandler(0, ..)`) that writes ONE line per distinct
//!     fault address to the bootstrap log for access violations, illegal /
//!     privileged instructions, stack overflow, fast fail, heap corruption,
//!     integer divide-by-zero, the first 4 distinct C++ exceptions, and so on.
//!     It then returns `EXCEPTION_CONTINUE_SEARCH`. It never swallows an
//!     exception and never changes the faulting thread's state.
//!
//! The handler is written to be safe *inside* a fault: no allocation, no locks,
//! no std formatting - a fixed stack buffer plus direct `WriteFile` on a log
//! handle opened at install time (a handle opened here would be a lock, a
//! heap allocation and a file open on an exhausted stack).

const std = @import("std");
const win = @import("win32.zig");
const state = @import("state.zig");
const logging = @import("logging.zig");
const minhook = @import("minhook.zig");

const verbose = logging.verbose;
const Atomic = std.atomic.Value;

/// Breadcrumb of the phase that is currently running, best effort (it is one
/// process-wide word, so with the render thread and the game thread both busy
/// it reports whichever phase stored last).
pub var STAGE: Atomic(u32) = .init(stage.IDLE);

pub const stage = struct {
    pub const IDLE: u32 = 0;
    pub const BLOCKTEMP_TICK: u32 = 1;
    pub const BLOCKTEMP_ENUMERATE: u32 = 2;
    pub const BLOCKTEMP_COLLECT_CELLS: u32 = 3;
    pub const BLOCKTEMP_PROBE: u32 = 4;
    pub const BLOCKTEMP_PUBLISH: u32 = 5;
    pub const FLIR_MESH_MAP: u32 = 6;
    pub const FLIR_BLOCK_DRAW: u32 = 7;
    pub const FLIR_COMPOSITE: u32 = 8;
    pub const HEAT_TEMPERATURE_AT: u32 = 9;
    pub const FLIR_TONEMAP: u32 = 10;
    pub const FLIR_GL_DRAW_ELEMENTS: u32 = 11;
    pub const FLIR_GL_UNIFORM_MATRIX: u32 = 12;
    pub const FLIR_GL_BIND_FRAMEBUFFER: u32 = 13;
};

/// Name of a stage code, for the fault line. Static strings, so the handler
/// needs no allocation.
pub fn stageName(code: u32) []const u8 {
    return switch (code) {
        stage.BLOCKTEMP_TICK => "blocktemp.tick",
        stage.BLOCKTEMP_ENUMERATE => "blocktemp enumerate",
        stage.BLOCKTEMP_COLLECT_CELLS => "blocktemp collect_cells",
        stage.BLOCKTEMP_PROBE => "blocktemp probe",
        stage.BLOCKTEMP_PUBLISH => "blocktemp publish",
        stage.FLIR_MESH_MAP => "flir mesh-map build",
        stage.FLIR_BLOCK_DRAW => "flir block draw",
        stage.FLIR_COMPOSITE => "flir composite",
        stage.HEAT_TEMPERATURE_AT => "heat.temperatureAt",
        stage.FLIR_TONEMAP => "flir tonemap hook",
        stage.FLIR_GL_DRAW_ELEMENTS => "glDrawElements thunk",
        stage.FLIR_GL_UNIFORM_MATRIX => "glUniformMatrix4fv thunk",
        stage.FLIR_GL_BIND_FRAMEBUFFER => "glBindFramebuffer thunk",
        else => "idle",
    };
}

/// Record the phase that is about to run (compiled out of the player build).
pub inline fn set(code: u32) void {
    if (verbose) STAGE.store(code, .monotonic);
}

/// Set the breadcrumb until the matching `leave` (use with `defer`), so a
/// phase with several returns never leaves the breadcrumb claiming a phase
/// that is over.
pub inline fn enter(code: u32) void {
    set(code);
}

pub inline fn leave() void {
    set(stage.IDLE);
}

/// Verbose builds: one lock-free line when the process exits through
/// ExitProcess (safe under the loader lock; other threads are already dead).
pub fn noteDetach(reserved_non_null: bool) void {
    if (!verbose) return;
    var line: Line = .{};
    line.str(if (reserved_non_null)
        "PROCESS EXIT: DLL_PROCESS_DETACH via ExitProcess, last stage="
    else
        "DLL unloaded (FreeLibrary), last stage=");
    line.str(stageName(STAGE.load(.monotonic)));
    line.str("\n");
    report(&line);
}

/// Install the fault handler on the bootstrap log. Verbose builds only: the
/// player build writes no diagnostics, so a fault line would be noise.
pub fn install(log_path: *const win.WidePath) void {
    if (!verbose) return;
    const handle = logging.openAppend(log_path.z()) orelse return;
    LOG_HANDLE.store(@intFromPtr(handle), .seq_cst);
    // first = 0: run after the game's own handlers, so this only sees what
    // nobody else handled.
    _ = win.AddVectoredExceptionHandler(0, &handler);
    hookTerminate();
}

// ---------------------------------------------------------------------------
// Vectored exception handler (verbose builds only)
// ---------------------------------------------------------------------------

// Exception codes the standard headers do not all name. `0xC000001D` is
// EXCEPTION_ILLEGAL_INSTRUCTION and `0xC00000FD` is EXCEPTION_STACK_OVERFLOW.
const STATUS_STACK_BUFFER_OVERRUN: u32 = 0xC0000409; // fast fail
const STATUS_HEAP_CORRUPTION: u32 = 0xC0000374;
const STATUS_INTEGER_DIVIDE_BY_ZERO: u32 = 0xC0000094;
const STATUS_PRIVILEGED_INSTRUCTION: u32 = 0xC0000096;
const STATUS_STACK_OVERFLOW: u32 = 0xC00000FD;
const STATUS_CPP_EXCEPTION: u32 = 0xE06D7363;

/// Append-only handle on the bootstrap log, opened once at install time so
/// the handler itself only has to `WriteFile`.
var LOG_HANDLE: Atomic(usize) = .init(0);
/// Re-entrancy guard: a fault inside the handler (a guarded page, a bad
/// stack) returns immediately instead of recursing.
var IN_HANDLER: Atomic(bool) = .init(false);
/// Fault addresses already reported, most recent last (one line each).
var SEEN: [8]Atomic(usize) = [_]Atomic(usize){.init(0)} ** 8;
var SEEN_NEXT: Atomic(usize) = .init(0);
/// The first 4 distinct C++ exception addresses (tracked separately so a
/// normal fault at the same address cannot hide one, and vice versa).
var CPP_SEEN: [4]Atomic(usize) = [_]Atomic(usize){.init(0)} ** 4;
var CPP_NEXT: Atomic(usize) = .init(0);

/// Little fixed-capacity ASCII line builder: no allocation, no formatting
/// machinery that could take a lock or touch the heap.
const Line = struct {
    bytes: [1024]u8 = undefined,
    len: usize = 0,

    fn str(self: *Line, text: []const u8) void {
        const take = @min(text.len, self.bytes.len - self.len);
        @memcpy(self.bytes[self.len..][0..take], text[0..take]);
        self.len += take;
    }

    fn byte(self: *Line, c: u8) void {
        if (self.len < self.bytes.len) {
            self.bytes[self.len] = c;
            self.len += 1;
        }
    }

    fn hex(self: *Line, value: usize) void {
        self.str("0x");
        var started = false;
        var shift: u6 = 15;
        while (true) : (shift -= 1) {
            const digit: u8 = @intCast((value >> (@as(u6, shift) * 4)) & 0xf);
            if (digit != 0) started = true;
            if (started or shift == 0) {
                self.byte(if (digit < 10) '0' + digit else 'a' + digit - 10);
            }
            if (shift == 0) break;
        }
    }

    fn dec(self: *Line, value: u64) void {
        var digits: [20]u8 = undefined;
        var count: usize = 0;
        var rest = value;
        while (true) {
            digits[count] = '0' + @as(u8, @intCast(rest % 10));
            count += 1;
            rest /= 10;
            if (rest == 0) break;
        }
        while (count > 0) {
            count -= 1;
            self.byte(digits[count]);
        }
    }
};

/// `game exe` / `our dll` / `other` plus the RVA inside that module.
fn module(addr: usize, line: *Line) void {
    const game = state.MODULE_BASE.load(.monotonic);
    const ours = state.SELF_MODULE.load(.monotonic);
    var info: win.MEMORY_BASIC_INFORMATION = .{};
    const queried = win.VirtualQuery(@ptrFromInt(addr), &info, @sizeOf(win.MEMORY_BASIC_INFORMATION));
    const image = queried != 0 and info.Type == win.MEM_IMAGE;
    const base: usize = if (image) @intFromPtr(info.AllocationBase) else 0;
    var name: []const u8 = "no image";
    var module_base: usize = 0;
    if (base != 0 and base == game) {
        name = "game exe";
        module_base = game;
    } else if (base != 0 and base == ours) {
        name = "our dll";
        module_base = ours;
    } else if (base != 0) {
        name = "other image";
        module_base = base;
    }
    line.str(name);
    if (module_base != 0 and module_base != game and module_base != ours) {
        line.str("@");
        line.hex(module_base);
    }
    if (module_base != 0) {
        line.str(" rva=");
        line.hex(addr -% module_base);
    } else {
        line.str(" addr=");
        line.hex(addr);
    }
}

fn report(line: *Line) void {
    const handle: win.HANDLE = @ptrFromInt(LOG_HANDLE.load(.monotonic));
    if (handle == null) return;
    var written: u32 = 0;
    _ = win.WriteFile(handle, &line.bytes, @intCast(line.len), &written, null);
    // One line per fault and a crash usually follows: get it out now rather
    // than trusting a later flush that may never happen.
    _ = win.FlushFileBuffers(handle);
}

const NtTerminate = *const fn (win.HANDLE, i32) callconv(.winapi) i32;
var NT_TERMINATE_ORIG: Atomic(usize) = .init(0);

/// Every process exit, including TerminateProcess / abort paths that never
/// reach DLL_PROCESS_DETACH: exit status plus the caller's stack.
fn ntTerminateDetour(handle: win.HANDLE, status: i32) callconv(.winapi) i32 {
    // handle 0 = "other threads" (first half of ExitProcess), -1 = self.
    const raw = @intFromPtr(handle);
    if (raw == std.math.maxInt(usize) or raw == 0) {
        var line: Line = .{};
        line.str("PROCESS TERMINATE: NtTerminateProcess handle=");
        line.hex(raw);
        line.str(" status=");
        line.hex(@as(u32, @bitCast(status)));
        line.str(" thread=");
        line.dec(win.GetCurrentThreadId());
        line.str(" stack:");
        var frames: [10]?*anyopaque = .{null} ** 10;
        const n = win.RtlCaptureStackBackTrace(1, 10, &frames, null);
        for (frames[0..@min(n, frames.len)]) |frame| {
            line.str(" ");
            module(@intFromPtr(frame), &line);
        }
        line.str("\n");
        report(&line);
    }
    const orig: NtTerminate = @ptrFromInt(NT_TERMINATE_ORIG.load(.monotonic));
    return orig(handle, status);
}

fn hookTerminate() void {
    const ntdll = win.GetModuleHandleA("ntdll.dll");
    const target = win.GetProcAddress(ntdll, "NtTerminateProcess") orelse return;
    var status: minhook.Status = undefined;
    if (minhook.createHook(@intFromPtr(target), &ntTerminateDetour, &status)) |orig| {
        NT_TERMINATE_ORIG.store(orig, .monotonic);
        _ = minhook.enableHook(@intFromPtr(target), &status);
    }
}

fn handler(info: ?*win.EXCEPTION_POINTERS) callconv(.winapi) i32 {
    if (IN_HANDLER.swap(true, .monotonic)) return win.EXCEPTION_CONTINUE_SEARCH;
    defer IN_HANDLER.store(false, .monotonic);
    const pointers = info orelse return win.EXCEPTION_CONTINUE_SEARCH;
    const record = pointers.ExceptionRecord orelse return win.EXCEPTION_CONTINUE_SEARCH;
    const code = record.ExceptionCode;
    // Only the codes worth a line: anything else belongs to the game / the
    // debugger (or is a first-chance C++/SEH exception that is handled).
    const label: []const u8 = switch (code) {
        win.EXCEPTION_ACCESS_VIOLATION => "access_violation",
        win.EXCEPTION_ILLEGAL_INSTRUCTION => "illegal_instruction",
        STATUS_STACK_OVERFLOW => "stack_overflow",
        STATUS_STACK_BUFFER_OVERRUN => "fast_fail",
        STATUS_HEAP_CORRUPTION => "heap_corruption",
        STATUS_INTEGER_DIVIDE_BY_ZERO => "integer_divide_by_zero",
        STATUS_PRIVILEGED_INSTRUCTION => "privileged_instruction",
        STATUS_CPP_EXCEPTION => "cpp_exception",
        // Anything else error-severity (invalid CRT parameter 0xC0000417,
        // in-page error, ...) or a breakpoint: a quiet abort path.
        else => if (code >= 0xC000_0000 or code == 0x8000_0003) "other" else return win.EXCEPTION_CONTINUE_SEARCH,
    };
    const cpp = code == STATUS_CPP_EXCEPTION;
    const address = @intFromPtr(record.ExceptionAddress);
    // One line per distinct fault address: a fault loop would otherwise
    // fill the log with the same record. C++ exceptions get their own
    // first-4 ring, since they are thrown and usually handled.
    const seen: []Atomic(usize) = if (cpp) &CPP_SEEN else &SEEN;
    for (seen) |*slot| {
        if (slot.load(.monotonic) == address and address != 0) return win.EXCEPTION_CONTINUE_SEARCH;
    }
    const next = if (cpp) &CPP_NEXT else &SEEN_NEXT;
    const index = next.fetchAdd(1, .monotonic);
    if (cpp) {
        // Strictly the first 4 distinct C++ exception sites: after that,
        // stay silent instead of cycling (throws are common and handled).
        if (index >= CPP_SEEN.len) return win.EXCEPTION_CONTINUE_SEARCH;
        CPP_SEEN[index].store(address, .monotonic);
    } else {
        SEEN[index % SEEN.len].store(address, .monotonic);
    }

    var line: Line = .{};
    line.str("FAULT ");
    line.str(label);
    line.str(" code=");
    line.hex(code);
    line.str(" at=");
    line.hex(address);
    line.str(" (");
    module(address, &line);
    line.str(")");
    // C++ exceptions carry a compiler-specific argument block: report the
    // faulting site only, never trust (or decode) that payload.
    if (code == win.EXCEPTION_ACCESS_VIOLATION and record.NumberParameters >= 2) {
        // ExceptionInformation[0]: 0 read, 1 write, 8 execute (DEP).
        line.str(" access=");
        line.str(switch (record.ExceptionInformation[0]) {
            0 => "read",
            1 => "write",
            8 => "execute",
            else => "flag",
        });
        line.str(" address=");
        line.hex(record.ExceptionInformation[1]);
    }
    line.str(" thread=");
    line.dec(win.GetCurrentThreadId());
    const current = STAGE.load(.monotonic);
    line.str(" stage=");
    line.str(stageName(current));
    line.str(" (");
    line.dec(current);
    line.str(")\n");
    report(&line);
    // Never swallow: the game's / debugger's own handler still gets it.
    return win.EXCEPTION_CONTINUE_SEARCH;
}
