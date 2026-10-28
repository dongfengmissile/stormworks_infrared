const std = @import("std");

pub fn build(b: *std.Build) void {
    // The plugin only exists for 64-bit Windows (the game is stormworks64.exe).
    // The GNU ABI matches the previous x86_64-pc-windows-gnu toolchain.
    const target = b.standardTargetOptions(.{
        .default_target = .{ .cpu_arch = .x86_64, .os_tag = .windows, .abi = .gnu },
    });
    // The hooks run inside every game draw; unoptimised code there costs real
    // FPS. ReleaseSafe keeps the safety checks (the old dev profile's debug
    // assertions) while optimising, so it is the default.
    const optimize = b.option(
        std.builtin.OptimizeMode,
        "optimize",
        "Prioritize performance, safety, or binary size (default: ReleaseSafe)",
    ) orelse .ReleaseSafe;

    // Verbose diagnostics: on for every mode except the player builds
    // (ReleaseFast / ReleaseSmall). INFRARED_LOGGING=1/0 forces it either way,
    // and -Dlogging overrides both.
    const forced: ?bool = if (b.graph.environ_map.get("INFRARED_LOGGING")) |value|
        (if (std.mem.eql(u8, value, "1")) true else if (std.mem.eql(u8, value, "0")) false else null)
    else
        null;
    const logging = b.option(bool, "logging", "Verbose diagnostics (default: on unless ReleaseFast/ReleaseSmall)") orelse
        forced orelse switch (optimize) {
        .Debug, .ReleaseSafe => true,
        .ReleaseFast, .ReleaseSmall => false,
    };

    // Unique stamp per compiled binary, logged at bootstrap. On-disk checksums
    // cannot tell you what a RUNNING game has loaded; this can.
    const stamp = std.Io.Clock.real.now(b.graph.io).toSeconds();

    const options = b.addOptions();
    options.addOption(bool, "logging", logging);
    options.addOption([]const u8, "build_stamp", b.fmt("{d}", .{stamp}));

    const minhook = b.dependency("minhook", .{});

    const dll_module = b.createModule(.{
        .root_source_file = b.path("src/main.zig"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
    });
    dll_module.addOptions("build_options", options);
    dll_module.addIncludePath(minhook.path("include"));
    dll_module.addCSourceFiles(.{
        .root = minhook.path("src"),
        .files = &.{ "buffer.c", "hook.c", "trampoline.c", "hde/hde64.c" },
        .flags = &.{ "-std=c11", "-fno-sanitize=undefined" },
    });
    dll_module.linkSystemLibrary("kernel32", .{});
    dll_module.linkSystemLibrary("opengl32", .{});

    const dll = b.addLibrary(.{
        .name = "StormworksInfrared",
        .linkage = .dynamic,
        .root_module = dll_module,
    });
    b.installArtifact(dll);

    const inject = b.addExecutable(.{
        .name = "infrared_inject",
        .root_module = b.createModule(.{
            .root_source_file = b.path("src/inject.zig"),
            .target = target,
            .optimize = optimize,
        }),
    });
    b.installArtifact(inject);

    // Pure unit tests (radiometry, the thermal step, the sun sprite fit, the Fx
    // hash spread). Nothing they call touches game memory.
    const test_module = b.createModule(.{
        .root_source_file = b.path("src/tests.zig"),
        .target = target,
        .optimize = .Debug,
        .link_libc = true,
    });
    test_module.addOptions("build_options", options);
    const tests = b.addTest(.{ .root_module = test_module });
    const test_step = b.step("test", "Run the unit tests");
    test_step.dependOn(&b.addRunArtifact(tests).step);
}
