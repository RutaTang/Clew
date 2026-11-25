defmodule Shop.Checkout do
  @moduledoc """
  Prices orders.
  """
  alias Shop.{Order, Repo}
  alias Shop.Pricing.Tax
  import Ecto.Query, only: [from: 2]
  use GenServer
  require Logger

  @type money :: float

  @doc """
  The order's total, with tax.

      iex> Shop.Checkout.price(%Order{total: 10.0})
      12.0
  """
  @spec price(Order.t()) :: money
  def price(%Order{total: total}) do
    total |> Tax.apply() |> round_to(2)
  end

  @doc "Rounds a price."
  def round_to(x, n) when is_float(x), do: Float.round(x, n)

  defp secret, do: :ok

  defmacro twice(x) do
    quote do: unquote(x) * 2
  end
end
