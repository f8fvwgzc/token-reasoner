-- Prices a shopping cart.
local money = require("money")

-- The prices of the items a customer picked.
local Cart = {}
Cart.__index = Cart

-- Adds up the item prices.
function Cart:subtotal()
  local sum = 0
  for _, price in ipairs(self.items) do
    sum = sum + price
  end
  return sum
end

-- Adds the sales tax to an amount.
local function apply_tax(amount)
  return amount * 108 // 100 -- NOTE: integer division keeps whole cents
end

-- The subtotal with tax.
function Cart:total()
  return apply_tax(self:subtotal())
end

-- Prices the cart and formats the result.
local function checkout(cart)
  local total = cart:total()
  return money.format_money(apply_tax(total))
end

return { Cart = Cart, checkout = checkout }
