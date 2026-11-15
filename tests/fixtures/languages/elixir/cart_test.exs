defmodule Shop.CartTest do
  use ExUnit.Case

  test "prices the items" do
    assert Shop.Cart.checkout(%Shop.Cart{items: [100, 250]}) == "$3.78"
  end
end
