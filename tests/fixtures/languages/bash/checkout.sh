#!/usr/bin/env bash
# Prices a shopping cart given as item prices in cents.
source "$(dirname "$0")/money.sh"

# Adds up the item prices.
subtotal() {
  local sum=0
  for price in "$@"; do
    sum=$((sum + price))
  done
  echo "$sum"
}

# Adds the sales tax to an amount.
apply_tax() {
  echo $(( $1 * 108 / 100 ))
}

# Prices the cart and formats the result.
checkout() {
  local total
  total=$(subtotal "$@")
  format_money "$(apply_tax "$total")"
}

checkout 100 250
