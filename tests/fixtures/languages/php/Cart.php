<?php

namespace Shop;

/** The prices of the items a customer picked. */
class Cart
{
    private array $items = [];

    /** Adds up the item prices. */
    public function subtotal(): int
    {
        return array_sum($this->items);
    }

    /** The subtotal with tax. */
    public function total(): int
    {
        return apply_tax($this->subtotal());
    }
}

/** Adds the sales tax to an amount. */
function apply_tax(int $amount): int
{
    return intdiv($amount * 108, 100);
}

/** Prices the cart and formats the result. */
function checkout(Cart $cart): string
{
    $total = $cart->total();
    return format_money(apply_tax($total));
}
