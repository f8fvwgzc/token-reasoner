# Prices a shopping cart.

# The prices of the items a customer picked.
class Cart
  def initialize(items)
    @items = items
  end

  # Adds up the item prices.
  def subtotal
    @items.sum
  end

  # The subtotal with tax.
  def total
    apply_tax(subtotal)
  end
end

# Adds the sales tax to an amount.
def apply_tax(amount)
  amount * 108 / 100 # TODO: read the rate from the store
end

# Prices the cart and formats the result.
def checkout(cart)
  total = cart.total
  format_money(apply_tax(total))
end
