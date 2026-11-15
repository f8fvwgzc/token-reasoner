// Prices a shopping cart.

/// The prices of the items a customer picked.
struct Cart {
    var items: [Int]

    /// Adds up the item prices.
    func subtotal() -> Int {
        return items.reduce(0, +)
    }

    /// The subtotal with tax.
    func total() -> Int {
        return applyTax(self.subtotal())
    }
}

/// Anything that has a total.
protocol Priced {
    func total() -> Int
}

/// Adds the sales tax to an amount.
func applyTax(_ amount: Int) -> Int {
    return amount * 108 / 100
}

/// Prices the cart and formats the result.
func checkout(_ cart: Cart) -> String {
    let total = cart.total()
    return formatMoney(applyTax(total))
}
