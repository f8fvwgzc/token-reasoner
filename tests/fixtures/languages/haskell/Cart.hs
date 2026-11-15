-- | Prices a shopping cart.
module Cart where

import qualified Data.List as List
import Money (formatMoney)

-- | The prices of the items a customer picked.
data Cart = Cart { items :: [Int] }

-- | Anything that has a total.
class Priced a where
  total :: a -> Int

instance Priced Cart where
  total cart = applyTax (subtotal cart)

-- | Adds up the item prices.
subtotal :: Cart -> Int
subtotal cart = List.foldl' (+) 0 (items cart)

-- | Adds the sales tax to an amount.
applyTax :: Int -> Int
applyTax amount = amount * 108 `div` 100 -- WHY: whole cents only

-- | Prices the cart and formats the result.
checkout :: Cart -> String
checkout cart = formatMoney (applyTax (total cart))
