//! Unit-test root: the pure parts of the plugin (radiometry, the thermal step,
//! the sun sprite fit, the Fx hash). Nothing here touches game memory.

const std = @import("std");

test {
    _ = @import("fx.zig");
    _ = @import("sun.zig");
    _ = @import("blocktemp.zig");
    _ = @import("ir.zig");
}

test "band radiance rises steeply with temperature" {
    const ir = @import("ir.zig");
    const warm = ir.bandRadiance(313.15);
    const furnace = ir.bandRadiance(673.15);
    try std.testing.expect(warm > 0.0);
    // A 400 C furnace outshines a 40 C engine by roughly 356x in-band.
    try std.testing.expect(furnace / warm > 300.0 and furnace / warm < 420.0);
    try std.testing.expectEqual(@as(f64, 0.0), ir.bandRadiance(50.0));
    // The 1 C table agrees with the integral between entries.
    try std.testing.expect(@abs(ir.bandRadianceC(40.0) / warm - 1.0) < 1e-6);
}
