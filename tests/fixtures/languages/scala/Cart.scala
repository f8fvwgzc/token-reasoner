package shop

/** The prices of the items a customer picked. */
class Cart(items: List[Int]) {
  /** Adds up the item prices. */
  def subtotal(): Int = items.sum

  /** The subtotal with tax. */
  def total(): Int = Checkout.applyTax(this.subtotal())
}

/** Prices carts. */
object Checkout {
  /** Adds the sales tax to an amount. */
  def applyTax(amount: Int): Int = amount * 108 / 100

  /** Prices the cart and formats the result. */
  def checkout(cart: Cart): String = {
    val total = cart.total()
    Money.formatMoney(applyTax(total))
  }
}
