# Prices a shopping cart.
module Shop

include("money.jl")

"The prices of the items a customer picked."
struct Cart
    items::Vector{Int}
end

"Adds up the item prices."
function subtotal(cart::Cart)
    return Base.sum(cart.items)
end

"Adds the sales tax to an amount."
function apply_tax(amount)
    return div(amount * 108, 100)
end

"The subtotal with tax."
total(cart::Cart) = apply_tax(subtotal(cart))

"Prices the cart and formats the result."
function checkout(cart::Cart)
    t = total(cart)
    return format_money(apply_tax(t))
end

end
