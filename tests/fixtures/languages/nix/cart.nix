# Prices a shopping cart.
{ lib, money }:

let
  # Adds up the item prices.
  subtotal = cart: lib.foldl' (sum: price: sum + price) 0 cart.items;

  # Adds the sales tax to an amount.
  applyTax = amount: amount * 108 / 100;

  # The subtotal with tax.
  total = cart: applyTax (subtotal cart);
in
{
  inherit subtotal total applyTax;

  # Prices the cart and formats the result.
  checkout = cart: money.formatMoney (applyTax (total cart));
}
