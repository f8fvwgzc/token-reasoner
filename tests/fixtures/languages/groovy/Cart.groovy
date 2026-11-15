package shop

/** The prices of the items a customer picked. */
class Cart {
    List<Integer> items = [];

    /** Adds up the item prices. */
    int subtotal() {
        return items.sum(0)
    }

    /** The subtotal with tax. */
    int total() {
        return Checkout.applyTax(this.subtotal())
    }
}

/** Prices carts. */
class Checkout {
    /** Adds the sales tax to an amount. */
    static int applyTax(int amount) {
        return amount * 108 / 100
    }

    /** Prices the cart and formats the result. */
    static String checkout(Cart cart) {
        int total = cart.total()
        return Money.formatMoney(applyTax(total))
    }
}
