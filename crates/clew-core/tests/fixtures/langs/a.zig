//! A shop.
const std = @import("std");
const util = @import("util.zig");

/// An item.
pub const Item = struct {
    name: []const u8,
    /// Its cost.
    cost: u32,

    /// The price.
    pub fn price(self: Item) u32 {
        return self.cost + 1;
    }
};

pub const Kind = enum { a, b };
pub const U = union(enum) { x: u8, y: u16 };

/// Total.
pub fn total(items: []const Item) u32 {
    var t: u32 = 0;
    for (items) |i| t += i.price();
    return t;
}

test "total" {}
