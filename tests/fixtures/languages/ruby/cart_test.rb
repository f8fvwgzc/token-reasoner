require "minitest/autorun"
require_relative "cart"

class CartTest < Minitest::Test
  def test_checkout
    assert_equal "$3.78", checkout(Cart.new([100, 250]))
  end
end
