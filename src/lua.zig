//! Thin wrappers over the Lua C API the game exposes to components.

const std = @import("std");
const memory = @import("memory.zig");
const offsets = @import("offsets.zig");

const gameFn = memory.gameFn;
const peek = memory.peek;
const poke = memory.poke;

pub const FnPair = extern struct {
    name: ?[*:0]const u8,
    func: usize,
};

/// A Lua C function as the game calls it.
pub const LuaCFunction = *const fn (lua: ?*anyopaque) callconv(.c) i32;

pub fn luaArgSlot(lua: usize, index: i32) usize {
    const f = gameFn(*const fn (usize, i32) callconv(.c) ?[*]u8, offsets.RVA_ARG_SLOT);
    return @intFromPtr(f(lua, index));
}

pub fn luaArgNumber(lua: usize, index: i32) ?f64 {
    const slot = luaArgSlot(lua, index);
    if (slot == 0) return null;
    return switch (peek(u32, slot + 8)) {
        3 => peek(f64, slot),
        0x13 => @floatFromInt(peek(i64, slot)),
        else => null,
    };
}

/// A boolean argument (Lua 5.3 tag 1) or a number treated as a flag.
///
/// `luaArgNumber` only accepts tags 3 and 0x13, so a Lua `true`/`false`
/// arrives there as `null` unless this is used. Any non-zero number counts as
/// true, so an older script can keep passing `1` / `0`.
pub fn luaArgBool(lua: usize, index: i32) ?bool {
    const slot = luaArgSlot(lua, index);
    if (slot == 0) return null;
    if (peek(u32, slot + 8) == 1) return peek(u32, slot) != 0;
    const value = luaArgNumber(lua, index) orelse return null;
    return value != 0.0;
}

pub fn luaStackTop(lua: usize) usize {
    return peek(usize, lua + 16);
}

pub fn luaPushNumber(lua: usize, value: f64) void {
    const top = luaStackTop(lua);
    poke(f64, top, value);
    poke(u32, top + 8, 3);
    poke(usize, lua + 16, top + 0x10);
}

pub fn luaPushBoolean(lua: usize, value: bool) void {
    const top = luaStackTop(lua);
    // Lua 5.3 stores a boolean as an int in the value union, tag 1.
    poke(u32, top, @intFromBool(value));
    poke(u32, top + 8, 1);
    poke(usize, lua + 16, top + 0x10);
}

pub fn luaPushTable(lua: usize) usize {
    const create = gameFn(*const fn (usize) callconv(.c) usize, offsets.RVA_CREATE_TABLE);
    const table = create(lua);
    const top = luaStackTop(lua);
    poke(usize, top, table);
    poke(u32, top + 8, 0x45);
    poke(usize, lua + 16, top + 0x10);
    return table;
}

pub fn luaRawseti(lua: usize, index: i64) void {
    const f = gameFn(*const fn (usize, i32, i64) callconv(.c) void, offsets.RVA_RAWSETI);
    f(lua, -2, index);
}

pub fn luaPushString(lua: usize, value: [:0]const u8) void {
    const f = gameFn(*const fn (usize, [*:0]const u8) callconv(.c) void, offsets.RVA_PUSH_STRING);
    f(lua, value.ptr);
}

/// Replicates `FUN_140332a50`'s component lookup: the component is carried as
/// an upvalue of every component-mod Lua function, reachable through the
/// pseudo-index `0xfff0b9d7`.
pub fn luaComponentHandle(lua: usize) usize {
    const slot = luaArgSlot(lua, offsets.FIRST_UPVALUE_INDEX);
    if (slot == 0) return 0;
    const tag = peek(u32, slot + 8) & 0xf;
    return switch (tag) {
        2 => peek(usize, slot),
        7 => peek(usize, slot) +% 0x28,
        else => 0,
    };
}

pub fn luaComponent(lua: usize) usize {
    const handle = luaComponentHandle(lua);
    if (handle == 0) return 0;
    return memory.readUsizeAt(handle);
}
