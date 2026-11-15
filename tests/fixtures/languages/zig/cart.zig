//! Prices a shopping cart.
const std = @import("std");
const money = @import("money.zig");

/// The prices of the items a customer picked.
pub const Cart = struct {
    items: []const u64,

    /// Adds up the item prices.
    pub fn subtotal(self: Cart) u64 {
        var sum: u64 = 0;
        for (self.items) |price| {
            sum += price;
        }
        return sum;
    }

    /// The subtotal with tax.
    pub fn total(self: Cart) u64 {
        return applyTax(self.subtotal());
    }
};

/// Adds the sales tax to an amount.
pub fn applyTax(amount: u64) u64 {
    return amount * 108 / 100;
}

/// Prices the cart and formats the result.
pub fn checkout(cart: Cart, buf: []u8) ![]u8 {
    const total = cart.total();
    return money.formatMoney(applyTax(total), buf);
}
