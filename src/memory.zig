//! Raw process memory helpers: resolve a game function by RVA, verify a
//! function still matches its recorded prologue, and read primitives.

const std = @import("std");
const win = @import("win32.zig");
const state = @import("state.zig");
const logging = @import("logging.zig");

/// Protections through which a write is legal.
const WRITE_PAGES: u32 = win.PAGE_READWRITE | win.PAGE_WRITECOPY | win.PAGE_EXECUTE_READWRITE | win.PAGE_EXECUTE_WRITECOPY;

/// Plausible userland pointer range for a game object: below `PTR_MIN` is the
/// null page / small integers, above `PTR_MAX` is non-canonical.
pub const PTR_MIN: usize = 0x10000;
pub const PTR_MAX: usize = 0x7fff_ffff_ffff;

/// `PTR_MIN <= p < PTR_MAX`.
pub inline fn inPtrRange(p: usize) bool {
    return p >= PTR_MIN and p < PTR_MAX;
}

/// A game function at `rva`, as a pointer of type `T`.
pub fn gameFn(comptime T: type, rva: usize) T {
    const address = state.MODULE_BASE.load(.seq_cst) +% rva;
    return @ptrFromInt(address);
}

pub fn bytesMatch(address: usize, expected: []const u8) bool {
    const actual: [*]const u8 = @ptrFromInt(address);
    return std.mem.eql(u8, actual[0..expected.len], expected);
}

/// This DLL's own path.
pub fn selfModulePath() ?win.WidePath {
    const module = state.SELF_MODULE.load(.seq_cst);
    if (module == 0) return null;
    var path: win.WidePath = .{};
    const len = win.GetModuleFileNameW(@ptrFromInt(module), &path.buf, path.buf.len);
    if (len == 0 or len >= path.buf.len - 1) return null;
    path.len = len;
    path.buf[len] = 0;
    return path;
}

// ---------------------------------------------------------------------------
// Primitive reads (Lua ABI: Stormworks' Lua 5.3, value = 16 bytes: value + u32 tag at +8)
// ---------------------------------------------------------------------------

// Unaligned: game offsets are not always aligned.
// Null/kernel/non-canonical addresses return 0 instead of faulting (cheap; freed memory still faults).
inline fn plausibleAddress(addr: usize) bool {
    return addr >= 0x10000 and addr < 0x7FFF_FFFF_0000;
}

/// Logs the first few garbage reads with their caller so the bad pointer source can be fixed.
fn badRead(addr: usize) void {
    @branchHint(.cold);
    const Count = struct {
        var value: std.atomic.Value(u32) = .init(0);
    };
    if (Count.value.fetchAdd(1, .monotonic) < 8) {
        logging.logf("BAD READ address=0x{x} from 0x{x}", .{ addr, @returnAddress() });
    }
}

pub inline fn readAt(comptime T: type, addr: usize) T {
    if (!plausibleAddress(addr)) {
        badRead(addr);
        return std.mem.zeroes(T);
    }
    const p: *align(1) const T = @ptrFromInt(addr);
    return p.*;
}

pub inline fn readUsizeAt(addr: usize) usize {
    return readAt(usize, addr);
}
pub inline fn readU32At(addr: usize) u32 {
    return readAt(u32, addr);
}
pub inline fn readF32At(addr: usize) f32 {
    return readAt(f32, addr);
}
pub inline fn readF64At(addr: usize) f64 {
    return readAt(f64, addr);
}

/// Unchecked unaligned read (the caller has verified the address).
pub inline fn peek(comptime T: type, addr: usize) T {
    const p: *align(1) const T = @ptrFromInt(addr);
    return p.*;
}

/// Unchecked unaligned write (the caller has verified the address).
pub inline fn poke(comptime T: type, addr: usize, value: T) void {
    const p: *align(1) T = @ptrFromInt(addr);
    p.* = value;
}

// ---------------------------------------------------------------------------
// Region checks
// ---------------------------------------------------------------------------

/// Best-effort check that `len` bytes at `address` are committed and readable,
/// so a hook can refuse to touch a pointer it did not get from a register
/// argument. Uses region descriptors rather than `IsBadReadPtr`, which
/// documents itself as unreliable when another thread mutates the address space
/// (the game rebuilds block containers while the scene walk runs).
///
/// The region is cached per CALL only: a region validated on an earlier tick can
/// already be decommitted (the game frees containers it rebuilt), and a stale
/// cache would let every later probe pass while the pages are unmapped.
pub fn readable(address: usize, len: usize) bool {
    if (region_cache.active) {
        if (std.math.add(usize, address, len)) |end| {
            for (region_cache.regions[0..region_cache.count]) |region| {
                if (region.base <= address and end <= region.end) return true;
            }
        } else |_| {}
    }
    return regionOk(address, len, false);
}

/// Readable regions verified during one synchronous game-thread pass (a sim
/// tick, a mesh-map walk). `VirtualQuery` is a syscall per probe and the sim
/// probes every component of every body; inside one pass on the game's own
/// thread the game cannot free those containers under us, so positive results
/// are reused until the scope ends. Outside a scope nothing is cached (see the
/// stale-cache note above).
const RegionCache = struct {
    active: bool = false,
    regions: [64]Region = undefined,
    count: usize = 0,

    const Region = struct { base: usize, end: usize };
};

threadlocal var region_cache: RegionCache = .{};

/// Enables the per-thread region cache until `end` (not reentrant-safe by
/// design: a nested scope just keeps the outer one's cache).
pub const RegionCacheScope = struct {
    outer: bool,

    pub fn begin() RegionCacheScope {
        const was = region_cache.active;
        region_cache.active = true;
        return .{ .outer = was };
    }

    pub fn end(self: RegionCacheScope) void {
        if (!self.outer) {
            region_cache.active = false;
            region_cache.count = 0;
        }
    }
};

fn rememberRegion(base: usize, end: usize) void {
    if (!region_cache.active) return;
    if (region_cache.count >= region_cache.regions.len) {
        std.mem.copyForwards(RegionCache.Region, region_cache.regions[0 .. region_cache.count - 1], region_cache.regions[1..region_cache.count]);
        region_cache.count -= 1;
    }
    region_cache.regions[region_cache.count] = .{ .base = base, .end = end };
    region_cache.count += 1;
}

/// Best-effort check that `len` bytes at `address` are committed, writable and
/// not guarded. A write through an unverified pointer faults inside the game's
/// own code, so anything this plugin writes into game memory (an out-parameter
/// of a callback the game invokes) must pass this first.
pub fn writable(address: usize, len: usize) bool {
    return regionOk(address, len, true);
}

fn regionOk(address: usize, len: usize, need_write: bool) bool {
    if (address < PTR_MIN) return false;
    const end = std.math.add(usize, address, len) catch return false;
    var cursor = address;
    var cached: ?RegionCache.Region = null;
    while (cursor < end) {
        if (cached) |region| {
            if (region.base <= cursor and cursor < region.end) {
                cursor = region.end;
                continue;
            }
        }
        var info: win.MEMORY_BASIC_INFORMATION = .{};
        const written = win.VirtualQuery(@ptrFromInt(cursor), &info, @sizeOf(win.MEMORY_BASIC_INFORMATION));
        if (written == 0 or info.State != win.MEM_COMMIT or info.Protect == 0) return false;
        if (info.Protect & (win.PAGE_NOACCESS | win.PAGE_GUARD) != 0) return false;
        if (need_write and info.Protect & WRITE_PAGES == 0) return false;
        const base = @intFromPtr(info.BaseAddress);
        const region_end = std.math.add(usize, base, info.RegionSize) catch return false;
        if (region_end <= cursor) return false;
        cached = .{ .base = base, .end = region_end };
        if (!need_write) rememberRegion(base, region_end);
        cursor = region_end;
    }
    return true;
}

/// True when `address` is a plausible dispatch pointer: inside a mapped module
/// image (`.exe`/`.dll` code or data) or at least on an executable page (a
/// driver may hand out entry points from non-image executable memory).
pub fn imageBacked(address: usize) bool {
    if (address < PTR_MIN) return false;
    var info: win.MEMORY_BASIC_INFORMATION = .{};
    const written = win.VirtualQuery(@ptrFromInt(address), &info, @sizeOf(win.MEMORY_BASIC_INFORMATION));
    if (written == 0 or info.State != win.MEM_COMMIT or info.Protect & (win.PAGE_NOACCESS | win.PAGE_GUARD) != 0) {
        return false;
    }
    const execute_pages = win.PAGE_EXECUTE | win.PAGE_EXECUTE_READ | win.PAGE_EXECUTE_READWRITE | win.PAGE_EXECUTE_WRITECOPY;
    return info.Type == win.MEM_IMAGE or info.Protect & execute_pages != 0;
}

/// Largest offset from the image base a game pointer may have. The shipping
/// image is far smaller; this only keeps a wild pointer out of the check below.
const GAME_IMAGE_SPAN: usize = 0x1000_0000;

/// True when `address` lies inside the main game image and is mapped as image
/// memory. Used to accept a game vtable / slot pointer and reject a stale or
/// foreign object before calling through it (a function pointer inside our own
/// DLL, or a heap object, must not be treated as game code).
pub fn inGameImage(address: usize) bool {
    const base = state.MODULE_BASE.load(.seq_cst);
    return base != 0 and address >= base and address - base < GAME_IMAGE_SPAN and imageBacked(address);
}
