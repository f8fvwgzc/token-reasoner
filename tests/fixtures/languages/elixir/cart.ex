defmodule Shop.Cart do
  @moduledoc "Prices a shopping cart."

  defstruct items: []

  @doc "Adds up the item prices."
  def subtotal(%Shop.Cart{items: items}) do
    Enum.sum(items)
  end

  @doc "The subtotal with tax."
  def total(cart) do
    apply_tax(subtotal(cart))
  end

  @doc "Adds the sales tax to an amount."
  def apply_tax(amount) do
    div(amount * 108, 100)
  end

  @doc "Prices the cart and formats the result."
  def checkout(cart) do
    total = total(cart)
    Shop.Money.format_money(apply_tax(total))
  end
end
