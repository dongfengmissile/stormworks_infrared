//! infrared injector.
//!
//! Loads `StormworksInfrared.dll` into a running `stormworks64.exe` process
//! the standard way: `VirtualAllocEx` + `WriteProcessMemory` a UTF-16 DLL path
//! into the target, then `CreateRemoteThread` with `LoadLibraryW` as the thread
//! start routine.
//!
//! The plugin DLL performs its own bootstrap (hook install) from `DllMain` via
//! a helper thread, so no further RPC is needed after injection.
//!
//! Usage:
//!
//! ```text
//! infrared_inject.exe                     # find stormworks64.exe, inject the sibling DLL
//! infrared_inject.exe --pid 12345         # inject into a specific process
//! infrared_inject.exe --dll C:\path\StormworksInfrared.dll
//! ```

const std = @import("std");
const win = @import("win32.zig");

const DEFAULT_PROCESS_NAME = "stormworks64.exe";
const DEFAULT_DLL_NAME = "StormworksInfrared.dll";
const INJECTION_TIMEOUT_MS: u32 = 15_000;
/// `LoadLibraryW` returns a 64-bit `HMODULE` truncated to the thread exit
/// code, so only "nonzero" is a meaningful success signal.
const LOAD_LIBRARY_FAILED_EXIT_CODE: u32 = 0;

const TH32CS_SNAPPROCESS: u32 = 0x2;
const PROCESS_CREATE_THREAD: u32 = 0x0002;
const PROCESS_VM_OPERATION: u32 = 0x0008;
const PROCESS_VM_READ: u32 = 0x0010;
const PROCESS_VM_WRITE: u32 = 0x0020;
const PROCESS_QUERY_INFORMATION: u32 = 0x0400;
const WAIT_OBJECT_0: u32 = 0;
const STD_OUTPUT_HANDLE: u32 = @bitCast(@as(i32, -11));
const STD_ERROR_HANDLE: u32 = @bitCast(@as(i32, -12));

const PROCESSENTRY32W = extern struct {
    dwSize: u32,
    cntUsage: u32,
    th32ProcessID: u32,
    th32DefaultHeapID: usize,
    th32ModuleID: u32,
    cntThreads: u32,
    th32ParentProcessID: u32,
    pcPriClassBase: i32,
    dwFlags: u32,
    szExeFile: [260]u16,
};

extern "kernel32" fn CreateToolhelp32Snapshot(flags: u32, pid: u32) callconv(.winapi) win.HANDLE;
extern "kernel32" fn Process32FirstW(snapshot: win.HANDLE, entry: *PROCESSENTRY32W) callconv(.winapi) win.BOOL;
extern "kernel32" fn Process32NextW(snapshot: win.HANDLE, entry: *PROCESSENTRY32W) callconv(.winapi) win.BOOL;
extern "kernel32" fn OpenProcess(access: u32, inherit: win.BOOL, pid: u32) callconv(.winapi) win.HANDLE;
extern "kernel32" fn VirtualAllocEx(process: win.HANDLE, address: ?*anyopaque, size: usize, kind: u32, protect: u32) callconv(.winapi) ?*anyopaque;
extern "kernel32" fn VirtualFreeEx(process: win.HANDLE, address: ?*anyopaque, size: usize, kind: u32) callconv(.winapi) win.BOOL;
extern "kernel32" fn WriteProcessMemory(process: win.HANDLE, address: ?*anyopaque, buffer: *const anyopaque, size: usize, written: *usize) callconv(.winapi) win.BOOL;
extern "kernel32" fn CreateRemoteThread(process: win.HANDLE, attributes: ?*anyopaque, stack: usize, start: *const anyopaque, parameter: ?*anyopaque, flags: u32, thread_id: ?*u32) callconv(.winapi) win.HANDLE;
extern "kernel32" fn WaitForSingleObject(handle: win.HANDLE, milliseconds: u32) callconv(.winapi) u32;
extern "kernel32" fn GetExitCodeThread(thread: win.HANDLE, code: *u32) callconv(.winapi) win.BOOL;
extern "kernel32" fn GetFullPathNameW(name: [*:0]const u16, size: u32, buffer: [*]u16, file_part: ?*?[*:0]u16) callconv(.winapi) u32;
extern "kernel32" fn GetCommandLineW() callconv(.winapi) [*:0]const u16;
extern "kernel32" fn GetStdHandle(which: u32) callconv(.winapi) win.HANDLE;
extern "kernel32" fn LocalFree(memory: ?*anyopaque) callconv(.winapi) ?*anyopaque;
extern "shell32" fn CommandLineToArgvW(command_line: [*:0]const u16, argc: *i32) callconv(.winapi) ?[*][*:0]u16;

fn writeTo(which: u32, comptime fmt: []const u8, args: anytype) void {
    var buf: [4096]u8 = undefined;
    const text = std.fmt.bufPrint(&buf, fmt, args) catch buf[0..];
    var written: u32 = 0;
    _ = win.WriteFile(GetStdHandle(which), text.ptr, @intCast(text.len), &written, null);
}

fn println(comptime fmt: []const u8, args: anytype) void {
    writeTo(STD_OUTPUT_HANDLE, fmt ++ "\n", args);
}

fn die(comptime fmt: []const u8, args: anytype) noreturn {
    writeTo(STD_ERROR_HANDLE, "infrared_inject: " ++ fmt ++ "\n", args);
    win.ExitProcess(1);
}

fn lastErrorMessage(buf: []u8, what: []const u8) []const u8 {
    return std.fmt.bufPrint(buf, "{s} failed (GetLastError={d})", .{ what, win.GetLastError() }) catch what;
}

/// A UTF-16 command-line argument as UTF-8, for printing and parsing.
fn utf8(buf: []u8, wide: []const u16) []const u8 {
    const path = win.WidePath.fromWide(wide);
    return path.utf8(buf);
}

const Options = struct {
    pid: ?u32 = null,
    process_name: []const u16 = std.unicode.utf8ToUtf16LeStringLiteral(DEFAULT_PROCESS_NAME),
    dll: ?win.WidePath = null,
};

fn parseArgs() Options {
    var options: Options = .{};
    var argc: i32 = 0;
    const argv = CommandLineToArgvW(GetCommandLineW(), &argc) orelse return options;
    // argv stays alive for the life of the process (process_name points into it).
    const args = argv[1..@intCast(@max(argc, 1))];
    var i: usize = 0;
    var text: [1024]u8 = undefined;
    while (i < args.len) : (i += 1) {
        const arg = std.mem.span(args[i]);
        const name = utf8(&text, arg);
        if (std.mem.eql(u8, name, "--pid")) {
            i += 1;
            if (i >= args.len) die("--pid needs a value", .{});
            const value = utf8(&text, std.mem.span(args[i]));
            options.pid = std.fmt.parseInt(u32, value, 10) catch |err| die("invalid --pid \"{s}\": {s}", .{ value, @errorName(err) });
        } else if (std.mem.eql(u8, name, "--process-name")) {
            i += 1;
            if (i >= args.len) die("--process-name needs a value", .{});
            options.process_name = std.mem.span(args[i]);
        } else if (std.mem.eql(u8, name, "--dll")) {
            i += 1;
            if (i >= args.len) die("--dll needs a value", .{});
            options.dll = win.WidePath.fromWide(std.mem.span(args[i]));
        } else if (std.mem.eql(u8, name, "--help") or std.mem.eql(u8, name, "-h")) {
            printUsage();
            win.ExitProcess(0);
        } else {
            die("unknown argument \"{s}\" (see --help)", .{name});
        }
    }
    return options;
}

fn printUsage() void {
    println("usage: infrared_inject.exe [--pid <pid>] [--process-name <name>] [--dll <path>]", .{});
    println("  default process name: {s}", .{DEFAULT_PROCESS_NAME});
    println("  default dll: <game dir>\\{s}", .{DEFAULT_DLL_NAME});
}

fn eqlIgnoreCaseAscii(a: []const u16, b: []const u16) bool {
    if (a.len != b.len) return false;
    for (a, b) |x, y| {
        const lx = if (x >= 'A' and x <= 'Z') x + 32 else x;
        const ly = if (y >= 'A' and y <= 'Z') y + 32 else y;
        if (lx != ly) return false;
    }
    return true;
}

fn findProcessId(process_name: []const u16) ?u32 {
    const snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if (!validHandle(snapshot)) return null;
    defer _ = win.CloseHandle(snapshot);
    var entry = std.mem.zeroes(PROCESSENTRY32W);
    entry.dwSize = @sizeOf(PROCESSENTRY32W);
    if (Process32FirstW(snapshot, &entry) == 0) return null;
    while (true) {
        const len = std.mem.indexOfScalar(u16, &entry.szExeFile, 0) orelse entry.szExeFile.len;
        if (eqlIgnoreCaseAscii(entry.szExeFile[0..len], process_name)) return entry.th32ProcessID;
        if (Process32NextW(snapshot, &entry) == 0) return null;
    }
}

fn validHandle(handle: win.HANDLE) bool {
    return handle != null and @intFromPtr(handle) != win.INVALID_HANDLE_VALUE;
}

fn defaultDllPath() ?win.WidePath {
    var exe: win.WidePath = .{};
    const len = win.GetModuleFileNameW(null, &exe.buf, exe.buf.len);
    if (len == 0 or len >= exe.buf.len - 1) return null;
    exe.len = len;
    exe.buf[len] = 0;
    return exe.parent().join(DEFAULT_DLL_NAME);
}

fn absolutePath(path: *const win.WidePath) ?win.WidePath {
    var out: win.WidePath = .{};
    const len = GetFullPathNameW(path.z(), out.buf.len, &out.buf, null);
    if (len == 0 or len >= out.buf.len) return null;
    out.len = len;
    out.buf[len] = 0;
    return out;
}

fn isFile(path: *const win.WidePath) bool {
    const attributes = win.GetFileAttributesW(path.z());
    return attributes != win.INVALID_FILE_ATTRIBUTES and attributes & win.FILE_ATTRIBUTE_DIRECTORY == 0;
}

fn run(err: []u8) ?[]const u8 {
    const options = parseArgs();
    var text: [1024]u8 = undefined;
    const pid = options.pid orelse findProcessId(options.process_name) orelse
        return std.fmt.bufPrint(err, "could not find a running process named \"{s}\"; start Stormworks first or pass --pid", .{utf8(&text, options.process_name)}) catch "process not found";

    const given = options.dll orelse defaultDllPath() orelse return "could not determine the plugin DLL path; pass --dll";
    if (!isFile(&given)) {
        return std.fmt.bufPrint(err, "plugin DLL not found: {s}", .{given.utf8(&text)}) catch "plugin DLL not found";
    }

    // Resolve the DLL path up front so injection never loads through a
    // relative path that depends on the target's working directory.
    const dll_path = absolutePath(&given) orelse
        return std.fmt.bufPrint(err, "resolving {s} failed (GetLastError={d})", .{ given.utf8(&text), win.GetLastError() }) catch "resolving the DLL path failed";

    println("injecting {s} into pid {d}", .{ dll_path.utf8(&text), pid });
    const dll_bytes_len = (dll_path.len + 1) * @sizeOf(u16);

    const process = OpenProcess(PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION | PROCESS_VM_OPERATION | PROCESS_VM_READ | PROCESS_VM_WRITE, 0, pid);
    if (process == null) return lastErrorMessage(err, "OpenProcess");
    defer _ = win.CloseHandle(process);

    const remote_path = VirtualAllocEx(process, null, dll_bytes_len, win.MEM_COMMIT | win.MEM_RESERVE, win.PAGE_READWRITE) orelse
        return lastErrorMessage(err, "VirtualAllocEx");
    defer _ = VirtualFreeEx(process, remote_path, 0, win.MEM_RELEASE);

    var written: usize = 0;
    if (WriteProcessMemory(process, remote_path, &dll_path.buf, dll_bytes_len, &written) == 0 or written != dll_bytes_len) {
        return lastErrorMessage(err, "WriteProcessMemory");
    }

    const kernel32 = win.GetModuleHandleW(std.unicode.utf8ToUtf16LeStringLiteral("kernel32.dll"));
    if (kernel32 == null) return lastErrorMessage(err, "GetModuleHandleW(kernel32)");
    const load_library = win.GetProcAddress(kernel32, "LoadLibraryW") orelse return "LoadLibraryW not found in kernel32.dll";

    const thread = CreateRemoteThread(process, null, 0, load_library, remote_path, 0, null);
    if (thread == null) return lastErrorMessage(err, "CreateRemoteThread");
    defer _ = win.CloseHandle(thread);

    if (WaitForSingleObject(thread, INJECTION_TIMEOUT_MS) != WAIT_OBJECT_0) {
        return std.fmt.bufPrint(err, "remote LoadLibraryW thread did not finish within {d} ms", .{INJECTION_TIMEOUT_MS}) catch "remote LoadLibraryW thread timed out";
    }

    var exit_code: u32 = 0;
    if (GetExitCodeThread(thread, &exit_code) == 0) return lastErrorMessage(err, "GetExitCodeThread");

    if (exit_code == LOAD_LIBRARY_FAILED_EXIT_CODE) {
        const log_hint = dll_path.parent().join("logs").join("infrared_bootstrap.log");
        return std.fmt.bufPrint(err, "remote LoadLibraryW returned 0; check {s} for details", .{log_hint.utf8(&text)}) catch "remote LoadLibraryW returned 0";
    }

    println("injection succeeded; the plugin bootstraps itself from DllMain", .{});
    return null;
}

pub fn main() void {
    var err: [2048]u8 = undefined;
    if (run(&err)) |message| die("{s}", .{message});
}
