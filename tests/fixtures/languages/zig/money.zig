const std = @import("std");

/// Renders cents as dollars.
pub fn formatMoney(cents: u64, buf: []u8) ![]u8 {
    return std.fmt.bufPrint(buf, "${d}.{d:0>2}", .{ cents / 100, cents % 100 });
}
