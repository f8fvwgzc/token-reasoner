# Formats amounts of money.
{ lib }:

{
  # Renders cents as dollars.
  formatMoney = cents: "$" + toString (cents / 100) + "." + lib.fixedWidthNumber 2 (lib.mod cents 100);
}
