//! The slice of the Win32 API this plugin uses, declared directly so the DLL
//! depends on nothing but kernel32/opengl32 (no `std.Io` runtime inside the
//! game process), plus the two primitives built on it: a static-initialisable
//! SRWLOCK mutex and the monotonic clock.

const std = @import("std");

pub const BOOL = i32;
pub const DWORD = u32;
pub const HANDLE = ?*anyopaque;
pub const HMODULE = ?*anyopaque;
pub const FARPROC = ?*const anyopaque;
pub const WINAPI: std.builtin.CallingConvention = .winapi;

pub const INVALID_HANDLE_VALUE: usize = std.math.maxInt(usize);

pub const MEM_COMMIT: u32 = 0x1000;
pub const MEM_RESERVE: u32 = 0x2000;
pub const MEM_RELEASE: u32 = 0x8000;
pub const MEM_IMAGE: u32 = 0x100_0000;

pub const PAGE_NOACCESS: u32 = 0x01;
pub const PAGE_READWRITE: u32 = 0x04;
pub const PAGE_WRITECOPY: u32 = 0x08;
pub const PAGE_EXECUTE: u32 = 0x10;
pub const PAGE_EXECUTE_READ: u32 = 0x20;
pub const PAGE_EXECUTE_READWRITE: u32 = 0x40;
pub const PAGE_EXECUTE_WRITECOPY: u32 = 0x80;
pub const PAGE_GUARD: u32 = 0x100;

pub const GENERIC_WRITE: u32 = 0x4000_0000;
pub const FILE_APPEND_DATA: u32 = 0x0004;
pub const FILE_SHARE_READ: u32 = 0x1;
pub const FILE_SHARE_WRITE: u32 = 0x2;
pub const FILE_SHARE_DELETE: u32 = 0x4;
pub const CREATE_ALWAYS: u32 = 2;
pub const OPEN_ALWAYS: u32 = 4;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
pub const INVALID_FILE_ATTRIBUTES: u32 = 0xffff_ffff;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;

pub const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
pub const EXCEPTION_ACCESS_VIOLATION: u32 = 0xC000_0005;
pub const EXCEPTION_ILLEGAL_INSTRUCTION: u32 = 0xC000_001D;
pub const EXCEPTION_STACK_OVERFLOW: u32 = 0xC000_00FD;

pub const MEMORY_BASIC_INFORMATION = extern struct {
    BaseAddress: ?*anyopaque = null,
    AllocationBase: ?*anyopaque = null,
    AllocationProtect: u32 = 0,
    PartitionId: u16 = 0,
    RegionSize: usize = 0,
    State: u32 = 0,
    Protect: u32 = 0,
    Type: u32 = 0,
};

pub const EXCEPTION_RECORD = extern struct {
    ExceptionCode: u32,
    ExceptionFlags: u32,
    ExceptionRecord: ?*EXCEPTION_RECORD,
    ExceptionAddress: ?*anyopaque,
    NumberParameters: u32,
    ExceptionInformation: [15]usize,
};

pub const EXCEPTION_POINTERS = extern struct {
    ExceptionRecord: ?*EXCEPTION_RECORD,
    ContextRecord: ?*anyopaque,
};

pub const VectoredHandler = *const fn (?*EXCEPTION_POINTERS) callconv(WINAPI) i32;
pub const ThreadProc = *const fn (?*anyopaque) callconv(WINAPI) u32;

pub const SRWLOCK = extern struct { ptr: ?*anyopaque = null };

pub extern "kernel32" fn GetModuleHandleW(name: ?[*:0]const u16) callconv(WINAPI) HMODULE;
pub extern "kernel32" fn GetModuleHandleA(name: ?[*:0]const u8) callconv(WINAPI) HMODULE;
pub extern "kernel32" fn GetModuleFileNameW(module: HMODULE, buffer: [*]u16, size: u32) callconv(WINAPI) u32;
pub extern "kernel32" fn GetProcAddress(module: HMODULE, name: [*:0]const u8) callconv(WINAPI) FARPROC;
pub extern "kernel32" fn LoadLibraryA(name: [*:0]const u8) callconv(WINAPI) HMODULE;
pub extern "kernel32" fn VirtualProtect(address: ?*anyopaque, size: usize, protect: u32, old: *u32) callconv(WINAPI) BOOL;
pub extern "user32" fn GetAsyncKeyState(key: i32) callconv(WINAPI) i16;
pub extern "kernel32" fn DisableThreadLibraryCalls(module: HMODULE) callconv(WINAPI) BOOL;
pub extern "kernel32" fn CreateThread(
    attributes: ?*anyopaque,
    stack_size: usize,
    start: ThreadProc,
    parameter: ?*anyopaque,
    flags: u32,
    thread_id: ?*u32,
) callconv(WINAPI) HANDLE;
pub extern "kernel32" fn Sleep(milliseconds: u32) callconv(WINAPI) void;
pub extern "kernel32" fn GetCurrentThreadId() callconv(WINAPI) u32;
pub extern "kernel32" fn GetLastError() callconv(WINAPI) u32;
pub extern "kernel32" fn CloseHandle(handle: HANDLE) callconv(WINAPI) BOOL;
pub extern "kernel32" fn CreateFileW(
    name: [*:0]const u16,
    access: u32,
    share: u32,
    security: ?*anyopaque,
    disposition: u32,
    flags: u32,
    template: HANDLE,
) callconv(WINAPI) HANDLE;
pub extern "kernel32" fn WriteFile(
    handle: HANDLE,
    buffer: [*]const u8,
    len: u32,
    written: ?*u32,
    overlapped: ?*anyopaque,
) callconv(WINAPI) BOOL;
pub extern "kernel32" fn FlushFileBuffers(handle: HANDLE) callconv(WINAPI) BOOL;
pub extern "kernel32" fn CreateDirectoryW(name: [*:0]const u16, security: ?*anyopaque) callconv(WINAPI) BOOL;
pub extern "kernel32" fn MoveFileExW(from: [*:0]const u16, to: [*:0]const u16, flags: u32) callconv(WINAPI) BOOL;
pub extern "kernel32" fn GetFileAttributesW(name: [*:0]const u16) callconv(WINAPI) u32;
pub extern "kernel32" fn GetEnvironmentVariableW(name: [*:0]const u16, buffer: ?[*]u16, size: u32) callconv(WINAPI) u32;
pub extern "kernel32" fn VirtualQuery(address: ?*const anyopaque, info: *MEMORY_BASIC_INFORMATION, len: usize) callconv(WINAPI) usize;
pub extern "kernel32" fn AddVectoredExceptionHandler(first: u32, handler: VectoredHandler) callconv(WINAPI) ?*anyopaque;
pub extern "kernel32" fn RtlCaptureStackBackTrace(skip: u32, count: u32, frames: [*]?*anyopaque, hash: ?*u32) callconv(WINAPI) u16;
pub extern "kernel32" fn QueryPerformanceCounter(count: *i64) callconv(WINAPI) BOOL;
pub extern "kernel32" fn QueryPerformanceFrequency(frequency: *i64) callconv(WINAPI) BOOL;
pub extern "kernel32" fn AcquireSRWLockExclusive(lock: *SRWLOCK) callconv(WINAPI) void;
pub extern "kernel32" fn ReleaseSRWLockExclusive(lock: *SRWLOCK) callconv(WINAPI) void;
pub extern "kernel32" fn ExitProcess(code: u32) callconv(WINAPI) noreturn;

// ---------------------------------------------------------------------------
// Mutex
// ---------------------------------------------------------------------------

/// A slim reader/writer lock used exclusively. Zero-initialised, so a global
/// `Mutex(T)` needs no runtime set-up, and it has no poisoning: the value is
/// always reachable, which is what every former `unwrap_or_else(into_inner)`
/// call site wanted anyway.
pub fn Mutex(comptime T: type) type {
    return struct {
        lock_word: SRWLOCK = .{},
        value: T,

        const Self = @This();

        pub fn lock(self: *Self) *T {
            AcquireSRWLockExclusive(&self.lock_word);
            return &self.value;
        }

        pub fn unlock(self: *Self) void {
            ReleaseSRWLockExclusive(&self.lock_word);
        }
    };
}

// ---------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------

var clock_start: std.atomic.Value(i64) = .init(0);
var clock_frequency: std.atomic.Value(i64) = .init(0);

fn ticks() i64 {
    var now: i64 = 0;
    _ = QueryPerformanceCounter(&now);
    return now;
}

/// Ticks since the first call (that call starts the clock) and the tick rate.
fn sinceStart() struct { elapsed: u64, frequency: u64 } {
    var frequency = clock_frequency.load(.monotonic);
    if (frequency == 0) {
        _ = QueryPerformanceFrequency(&frequency);
        if (frequency <= 0) frequency = 1;
        clock_frequency.store(frequency, .monotonic);
    }
    const now = ticks();
    var start = clock_start.load(.monotonic);
    if (start == 0) {
        start = clock_start.cmpxchgStrong(0, now, .monotonic, .monotonic) orelse now;
    }
    const elapsed: u64 = if (now > start) @intCast(now - start) else 0;
    return .{ .elapsed = elapsed, .frequency = @intCast(frequency) };
}

/// Milliseconds since the plugin's clock started.
pub fn elapsedMs() u64 {
    const t = sinceStart();
    return @intCast(@as(u128, t.elapsed) * 1000 / t.frequency);
}

/// Microseconds since the plugin's clock started.
pub fn elapsedUs() u64 {
    const t = sinceStart();
    return @intCast(@as(u128, t.elapsed) * 1_000_000 / t.frequency);
}

/// Nanoseconds since the plugin's clock started (cost accounting).
pub fn elapsedNs() u64 {
    const t = sinceStart();
    return @intCast(@as(u128, t.elapsed) * 1_000_000_000 / t.frequency);
}

// ---------------------------------------------------------------------------
// UTF-16 paths
// ---------------------------------------------------------------------------

/// Fixed-capacity NUL-terminated UTF-16 path, enough for any path the plugin
/// builds (the game directory plus a short file name).
pub const WidePath = struct {
    buf: [1024]u16 = undefined,
    len: usize = 0,

    pub fn fromWide(text: []const u16) WidePath {
        var path: WidePath = .{};
        path.appendWide(text);
        return path;
    }

    pub fn appendWide(self: *WidePath, text: []const u16) void {
        const take = @min(text.len, self.buf.len - 1 - self.len);
        @memcpy(self.buf[self.len..][0..take], text[0..take]);
        self.len += take;
        self.buf[self.len] = 0;
    }

    /// Append ASCII (every name this plugin appends is ASCII).
    pub fn appendAscii(self: *WidePath, text: []const u8) void {
        for (text) |c| {
            if (self.len + 1 >= self.buf.len) break;
            self.buf[self.len] = c;
            self.len += 1;
        }
        self.buf[self.len] = 0;
    }

    /// `self` + `\` + `name`.
    pub fn join(self: WidePath, name: []const u8) WidePath {
        var path = self;
        if (path.len > 0 and path.buf[path.len - 1] != '\\' and path.buf[path.len - 1] != '/') {
            path.appendAscii("\\");
        }
        path.appendAscii(name);
        return path;
    }

    /// Everything before the last path separator ("." when there is none).
    pub fn parent(self: WidePath) WidePath {
        var index = self.len;
        while (index > 0) {
            index -= 1;
            if (self.buf[index] == '\\' or self.buf[index] == '/') {
                return WidePath.fromWide(self.buf[0..index]);
            }
        }
        var path: WidePath = .{};
        path.appendAscii(".");
        return path;
    }

    pub fn z(self: *const WidePath) [*:0]const u16 {
        return @ptrCast(&self.buf);
    }

    pub fn slice(self: *const WidePath) []const u16 {
        return self.buf[0..self.len];
    }

    pub fn exists(self: *const WidePath) bool {
        return GetFileAttributesW(self.z()) != INVALID_FILE_ATTRIBUTES;
    }

    /// The path as UTF-8 for log lines (lossy for unpaired surrogates).
    pub fn utf8(self: *const WidePath, out: []u8) []const u8 {
        var len: usize = 0;
        var it = std.unicode.Utf16LeIterator.init(self.slice());
        while (true) {
            const cp = it.nextCodepoint() catch 0xfffd orelse break;
            var tmp: [4]u8 = undefined;
            const n = std.unicode.utf8Encode(cp, &tmp) catch 0;
            if (len + n > out.len) break;
            @memcpy(out[len..][0..n], tmp[0..n]);
            len += n;
        }
        return out[0..len];
    }
};

/// True when the environment variable `name` (ASCII) is exactly "1".
pub fn envIsOne(comptime name: []const u8) bool {
    const wide = comptime blk: {
        var w: [name.len:0]u16 = undefined;
        for (name, 0..) |c, i| w[i] = c;
        break :blk w;
    };
    var value: [4]u16 = undefined;
    const len = GetEnvironmentVariableW(&wide, &value, value.len);
    return len == 1 and value[0] == '1';
}
