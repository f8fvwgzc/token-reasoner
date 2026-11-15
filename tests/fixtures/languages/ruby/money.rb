# Formats amounts of money.

# Renders cents as dollars.
def format_money(cents)
  format("$%d.%02d", cents / 100, cents % 100)
end
