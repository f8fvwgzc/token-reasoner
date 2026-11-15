module Money exposing (formatMoney)


{-| Renders cents as dollars.
-}
formatMoney : Int -> String
formatMoney cents =
    "$" ++ String.fromInt (cents // 100) ++ "." ++ String.padLeft 2 '0' (String.fromInt (modBy 100 cents))
