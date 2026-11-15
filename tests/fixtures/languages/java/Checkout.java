package shop;

import java.util.ArrayList;
import java.util.List;

/** The prices of the items a customer picked. */
class Cart {
    private final List<Integer> items = new ArrayList<>();

    /** Adds up the item prices. */
    int subtotal() {
        int sum = 0;
        for (int price : items) {
            sum += price;
        }
        return sum;
    }

    /** The subtotal with tax. */
    int total() {
        return Checkout.applyTax(this.subtotal());
    }
}

/** Prices carts. */
public class Checkout {
    /** Adds the sales tax to an amount. */
    static int applyTax(int amount) {
        return amount * 108 / 100;
    }

    /** Prices the cart and formats the result. */
    static String checkout(Cart cart) {
        int total = cart.total();
        return Money.formatMoney(applyTax(total));
    }
}
