//! Conversions with the semantics the original code relied on, and the small
//! formatting adapters the log lines need.
//!
//! Every value here can come from game memory, so nothing may trip a safety
//! check: a panic inside a detour takes the game down with it.

const std = @import("std");

/// Float -> integer with saturation, NaN -> 0 (never illegal behaviour).
pub fn f2i(comptime T: type, value: anytype) T {
    const x: f64 = @floatCast(value);
    if (std.math.isNan(x)) return 0;
    const lo: f64 = @floatFromInt(std.math.minInt(T));
    const hi: f64 = @floatFromInt(std.math.maxInt(T));
    if (x <= lo) return std.math.minInt(T);
    if (x >= hi) return std.math.maxInt(T);
    return @intFromFloat(x);
}

/// Integer -> f64.
pub inline fn f(value: anytype) f64 {
    return @floatFromInt(value);
}

/// Integer -> f32.
pub inline fn f32_(value: anytype) f32 {
    return @floatFromInt(value);
}

pub fn clamp(value: anytype, lo: @TypeOf(value), hi: @TypeOf(value)) @TypeOf(value) {
    return @max(lo, @min(hi, value));
}

/// `f64::max` semantics: a NaN operand yields the other operand.
pub fn fmax(a: anytype, b: @TypeOf(a)) @TypeOf(a) {
    if (std.math.isNan(a)) return b;
    if (std.math.isNan(b)) return a;
    return @max(a, b);
}

/// `f64::min` semantics: a NaN operand yields the other operand.
pub fn fmin(a: anytype, b: @TypeOf(a)) @TypeOf(a) {
    if (std.math.isNan(a)) return b;
    if (std.math.isNan(b)) return a;
    return @min(a, b);
}

/// `f64::clamp` (NaN passes through as NaN).
pub fn fclamp(value: anytype, lo: @TypeOf(value), hi: @TypeOf(value)) @TypeOf(value) {
    if (std.math.isNan(value)) return value;
    return @max(lo, @min(hi, value));
}

pub inline fn isFinite(value: anytype) bool {
    return std.math.isFinite(value);
}

pub inline fn degrees(rad: f64) f64 {
    return rad * (180.0 / std.math.pi);
}

pub inline fn radians(deg: f64) f64 {
    return deg * (std.math.pi / 180.0);
}

/// `f64::round` (half away from zero).
pub inline fn round(x: f64) f64 {
    return @round(x);
}

pub inline fn sq(x: f64) f64 {
    return x * x;
}

// ---------------------------------------------------------------------------
// formatting
// ---------------------------------------------------------------------------

/// A float printed with an explicit sign (`{:+.3}`): `Signed(3){ .v = x }`
/// formats through `{f}`.
pub fn Signed(comptime precision: usize) type {
    return struct {
        v: f64,
        pub fn format(self: @This(), writer: *std.Io.Writer) std.Io.Writer.Error!void {
            if (!(self.v < 0) and !std.math.isNan(self.v)) try writer.writeByte('+');
            try writer.printFloat(self.v, .{ .mode = .decimal, .precision = precision });
        }
    };
}

pub fn signed(comptime precision: usize, value: anytype) Signed(precision) {
    return .{ .v = @floatCast(value) };
}

/// An optional world position, printed like the original `{:?}`:
/// `Some((x, y, z))` or `None`.
pub const OptPos = struct {
    v: ?[3]f64,
    pub fn format(self: OptPos, writer: *std.Io.Writer) std.Io.Writer.Error!void {
        if (self.v) |p| {
            try writer.print("Some(({d}, {d}, {d}))", .{ p[0], p[1], p[2] });
        } else {
            try writer.writeAll("None");
        }
    }
};

/// An optional address printed as hex, or `None`.
pub const OptHex = struct {
    v: ?usize,
    pub fn format(self: OptHex, writer: *std.Io.Writer) std.Io.Writer.Error!void {
        if (self.v) |v| try writer.print("Some(0x{x})", .{v}) else try writer.writeAll("None");
    }
};

/// A list of integers printed like `[1, 2, 3]` (hex when `hex`).
pub fn List(comptime T: type, comptime hex: bool) type {
    return struct {
        items: []const T,
        pub fn format(self: @This(), writer: *std.Io.Writer) std.Io.Writer.Error!void {
            try writer.writeByte('[');
            for (self.items, 0..) |item, i| {
                if (i != 0) try writer.writeAll(", ");
                if (hex) try writer.print("0x{x}", .{item}) else try writer.print("{d}", .{item});
            }
            try writer.writeByte(']');
        }
    };
}
