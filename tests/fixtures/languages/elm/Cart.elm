module Cart exposing (Cart, checkout, total)

import Money exposing (formatMoney)


{-| The prices of the items a customer picked.
-}
type alias Cart =
    { items : List Int }


{-| Adds up the item prices.
-}
subtotal : Cart -> Int
subtotal cart =
    List.sum cart.items


{-| The subtotal with tax.
-}
total : Cart -> Int
total cart =
    applyTax (subtotal cart)


{-| Adds the sales tax to an amount.
-}
applyTax : Int -> Int
applyTax amount =
    amount * 108 // 100


{-| Prices the cart and formats the result.
-}
checkout : Cart -> String
checkout cart =
    formatMoney (applyTax (total cart))
