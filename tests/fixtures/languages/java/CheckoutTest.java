package shop;

import static org.junit.jupiter.api.Assertions.assertEquals;

import org.junit.jupiter.api.Test;

class CheckoutTest {
    @Test
    void pricesAnEmptyCart() {
        assertEquals("$0.00", Checkout.checkout(new Cart()));
    }
}
