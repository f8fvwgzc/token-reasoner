# Prices a shopping cart.
source("money.R")

# The prices of the items a customer picked.
Cart <- setRefClass("Cart",
  fields = list(items = "numeric"),
  methods = list(
    subtotal = function() {
      sum(items)
    },
    total = function() {
      apply_tax(.self$subtotal())
    }
  )
)

# Adds the sales tax to an amount.
apply_tax <- function(amount) {
  amount * 108 %/% 100
}

# Prices the cart and formats the result.
checkout <- function(cart) {
  total <- cart$total()
  format_money(apply_tax(total))
}
