//! FxHash (rustc's) maps: the sim and the draw hook hash small integer keys
//! (cells, bodies, GL ids) many times per tick/draw, and a DoS-resistant hash
//! buys nothing for keys taken from game memory or GL.

const std = @import("std");

const SEED: u64 = 0x517c_c1b7_2722_0a95;

pub const Hasher = struct {
    state: u64 = 0,

    pub inline fn add(self: *Hasher, word: u64) void {
        self.state = (std.math.rotl(u64, self.state, 5) ^ word) *% SEED;
    }

    pub inline fn finish(self: Hasher) u64 {
        // The multiply leaves aligned keys (pointers) with zero low bits, and
        // the table indexes by low bits: rotate high bits down (rustc-hash 2).
        return std.math.rotl(u64, self.state, 26);
    }
};

/// Hash any integer, or a struct/array of integers, one word per field.
pub fn hash(key: anytype) u64 {
    var h: Hasher = .{};
    addKey(&h, key);
    return h.finish();
}

fn addKey(h: *Hasher, key: anytype) void {
    const T = @TypeOf(key);
    switch (@typeInfo(T)) {
        .int => |info| {
            const Unsigned = std.meta.Int(.unsigned, info.bits);
            h.add(@as(u64, @as(Unsigned, @bitCast(key))));
        },
        .bool => h.add(@intFromBool(key)),
        .@"enum" => addKey(h, @intFromEnum(key)),
        .array => for (key) |item| addKey(h, item),
        .@"struct" => |info| inline for (info.fields) |field| addKey(h, @field(key, field.name)),
        else => @compileError("fx.hash: unsupported key type " ++ @typeName(T)),
    }
}

pub fn Context(comptime K: type) type {
    return struct {
        pub fn hash(_: @This(), key: K) u64 {
            return @import("fx.zig").hash(key);
        }
        pub fn eql(_: @This(), a: K, b: K) bool {
            return std.meta.eql(a, b);
        }
    };
}

pub fn HashMap(comptime K: type, comptime V: type) type {
    return std.HashMapUnmanaged(K, V, Context(K), std.hash_map.default_max_load_percentage);
}

pub fn HashSet(comptime K: type) type {
    return HashMap(K, void);
}

test "aligned pointers spread over low bits" {
    var seen = [_]bool{false} ** 256;
    var distinct: usize = 0;
    for (0..256) |i| {
        const low: usize = @intCast(hash(@as(usize, 0x1_0000_0000 + i * 16)) & 0xff);
        if (!seen[low]) {
            seen[low] = true;
            distinct += 1;
        }
    }
    try std.testing.expect(distinct > 128);
}
