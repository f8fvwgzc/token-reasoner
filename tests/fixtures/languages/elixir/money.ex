defmodule Shop.Money do
  @moduledoc "Formats amounts of money."

  @doc "Renders cents as dollars."
  def format_money(cents) do
    "$#{div(cents, 100)}.#{String.pad_leading(Integer.to_string(rem(cents, 100)), 2, "0")}"
  end
end
