-- | Formats amounts of money.
module Money (formatMoney) where

import Text.Printf (printf)

-- | Renders cents as dollars.
formatMoney :: Int -> String
formatMoney cents = printf "$%d.%02d" (cents `div` 100) (cents `mod` 100)
