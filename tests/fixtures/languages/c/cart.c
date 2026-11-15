/* Prices a shopping cart. */
#include <stddef.h>

void format_money(int cents, char *out);

/* The prices of the items a customer picked. */
struct Cart {
    const int *items;
    size_t count;
    int (*total)(const struct Cart *cart);
};

/* Adds up the item prices. */
static int subtotal(const struct Cart *cart) {
    int sum = 0;
    for (size_t i = 0; i < cart->count; i++) {
        sum += cart->items[i];
    }
    return sum;
}

/* Adds the sales tax to an amount. */
int apply_tax(int amount) {
    return amount * 108 / 100;
}

/* Prices the cart and formats the result. */
void checkout(const struct Cart *cart, char *out) {
    int total = cart->total(cart) + subtotal(cart);
    format_money(apply_tax(total), out);
}
