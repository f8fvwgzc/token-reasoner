//// Prices a shopping cart.

import gleam/int
import gleam/list
import money

/// The prices of the items a customer picked.
pub type Cart {
  Cart(items: List(Int))
}

/// Adds up the item prices.
pub fn subtotal(cart: Cart) -> Int {
  list.fold(cart.items, 0, int.add)
}

/// Adds the sales tax to an amount.
pub fn apply_tax(amount: Int) -> Int {
  amount * 108 / 100
}

/// The subtotal with tax.
pub fn total(cart: Cart) -> Int {
  apply_tax(subtotal(cart))
}

/// Prices the cart and formats the result.
pub fn checkout(cart: Cart) -> String {
  let total = total(cart)
  money.format_money(apply_tax(total))
}
