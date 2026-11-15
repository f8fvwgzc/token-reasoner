/// Formats amounts of money.
module Shop.Money

/// Renders cents as dollars.
let formatMoney (cents: int) = sprintf "$%d.%02d" (cents / 100) (cents % 100)
