{-# LANGUAGE CPP #-}
-- | A shop.
module Shop.Price (price, Item(..)) where

import qualified Data.Map as M
import Data.List (sort)

-- | An item.
data Item = Item { name :: String, cost :: Int } deriving Show

newtype Money = Money Int

type Name = String

-- | Things with a price.
class Priced a where
  priceOf :: a -> Int

instance Priced Item where
  priceOf = cost

-- | The price.
price :: Item -> Int
price i = cost i + length "tax"
