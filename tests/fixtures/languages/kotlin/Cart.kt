package shop

/** The prices of the items a customer picked. */
class Cart(private val items: List<Int>) {
    /** Adds up the item prices. */
    fun subtotal(): Int = items.sum()

    /** The subtotal with tax. */
    fun total(): Int {
        return applyTax(this.subtotal())
    }
}

/** Adds the sales tax to an amount. */
fun applyTax(amount: Int): Int = amount * 108 / 100

/** Prices the cart and formats the result. */
fun checkout(cart: Cart): String {
    val total = cart.total()
    return formatMoney(applyTax(total))
}
