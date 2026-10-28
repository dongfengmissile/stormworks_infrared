//! MinHook (compiled from C by build.zig): create a detour, get the trampoline
//! that calls the original, enable it. Initialised lazily on first use.

const std = @import("std");

const MH_STATUS = c_int;
const MH_OK: MH_STATUS = 0;
const MH_ERROR_ALREADY_INITIALIZED: MH_STATUS = 1;

extern fn MH_Initialize() callconv(.winapi) MH_STATUS;
extern fn MH_CreateHook(target: ?*anyopaque, detour: ?*const anyopaque, original: *?*anyopaque) callconv(.winapi) MH_STATUS;
extern fn MH_EnableHook(target: ?*anyopaque) callconv(.winapi) MH_STATUS;
extern fn MH_StatusToString(status: MH_STATUS) callconv(.winapi) [*:0]const u8;

var init_lock: std.atomic.Value(u8) = .init(0); // 0 = not yet, 1 = running, 2 = done
var init_status: MH_STATUS = MH_OK;

pub const Status = struct {
    code: MH_STATUS,
    pub fn format(self: Status, writer: *std.Io.Writer) std.Io.Writer.Error!void {
        try writer.writeAll(std.mem.span(MH_StatusToString(self.code)));
    }
};

fn initialize() ?Status {
    while (true) {
        switch (init_lock.cmpxchgStrong(0, 1, .acquire, .acquire) orelse 0) {
            0 => {
                const status = MH_Initialize();
                init_status = if (status == MH_ERROR_ALREADY_INITIALIZED) MH_OK else status;
                init_lock.store(2, .release);
                break;
            },
            1 => std.atomic.spinLoopHint(),
            else => break,
        }
    }
    return if (init_status == MH_OK) null else .{ .code = init_status };
}

/// Create a (disabled) hook on `target`; returns the trampoline to the original.
pub fn createHook(target: usize, detour: *const anyopaque, status_out: *Status) ?usize {
    if (initialize()) |status| {
        status_out.* = status;
        return null;
    }
    var original: ?*anyopaque = null;
    const status = MH_CreateHook(@ptrFromInt(target), detour, &original);
    if (status != MH_OK) {
        status_out.* = .{ .code = status };
        return null;
    }
    return @intFromPtr(original);
}

pub fn enableHook(target: usize, status_out: *Status) bool {
    const status = MH_EnableHook(@ptrFromInt(target));
    if (status != MH_OK) {
        status_out.* = .{ .code = status };
        return false;
    }
    return true;
}
