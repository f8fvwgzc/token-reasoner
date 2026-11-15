# Formats amounts of money.

# Renders cents as dollars.
format_money <- function(cents) {
  base::sprintf("$%d.%02d", cents %/% 100, cents %% 100)
}
