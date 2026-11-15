/// Prices a shopping cart.
module Shop.Cart

open Shop.Money

/// Adds the sales tax to an amount.
let applyTax (amount: int) = amount * 108 / 100

/// The prices of the items a customer picked.
type Cart(items: int list) =
    /// Adds up the item prices.
    member this.Subtotal() = List.sum items

    /// The subtotal with tax.
    member this.Total() = applyTax (this.Subtotal())

/// Prices the cart and formats the result.
let checkout (cart: Cart) =
    let total = cart.Total()
    formatMoney (applyTax total)
