--!strict
-- Prices a shopping cart.
local money = require("./money")

-- The prices of the items a customer picked.
local Cart = {}
Cart.__index = Cart

export type Cart = typeof(setmetatable({} :: { items: { number } }, Cart))

-- Adds up the item prices.
function Cart.subtotal(self: Cart): number
	local sum = 0
	for _, price in self.items do
		sum += price
	end
	return sum
end

-- Adds the sales tax to an amount.
local function applyTax(amount: number): number
	return amount * 108 // 100
end

-- The subtotal with tax.
function Cart.total(self: Cart): number
	return applyTax(self:subtotal())
end

-- Prices the cart and formats the result.
local function checkout(cart: Cart): string
	local total = cart:total()
	return money.formatMoney(applyTax(total))
end

return { Cart = Cart, checkout = checkout }
