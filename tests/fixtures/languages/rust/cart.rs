//! Prices a shopping cart.

use crate::money::format_money;

/// The prices of the items a customer picked.
pub struct Cart {
    items: Vec<u64>,
}

impl Cart {
    /// An empty cart.
    pub fn new() -> Self {
        Cart { items: Vec::new() }
    }

    /// Adds up the item prices.
    pub fn subtotal(&self) -> u64 {
        self.items.iter().sum()
    }

    /// The subtotal with tax.
    pub fn total(&self) -> u64 {
        apply_tax(self.subtotal())
    }
}

/// Adds the sales tax to an amount.
pub fn apply_tax(amount: u64) -> u64 {
    amount * 108 / 100
}

/// Prices the cart and formats the result.
pub fn checkout(cart: &Cart) -> String {
    let total = cart.total();
    format_money(apply_tax(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checkout() {
        assert_eq!(checkout(&Cart::new()), "$0.00");
    }
}
