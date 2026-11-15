# Formats amounts of money.

# Renders cents as dollars.
function format_money {
  printf '$%d.%02d\n' $(( $1 / 100 )) $(( $1 % 100 ))
}
