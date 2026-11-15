using Printf

"Renders cents as dollars."
function format_money(cents)
    return @sprintf("\$%d.%02d", div(cents, 100), rem(cents, 100))
end
