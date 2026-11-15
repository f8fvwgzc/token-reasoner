package shop

import kotlin.test.Test
import kotlin.test.assertEquals

class CartTest {
    @Test
    fun pricesTheItems() {
        assertEquals("$3.78", checkout(Cart(listOf(100, 250))))
    }
}
