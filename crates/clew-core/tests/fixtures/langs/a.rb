# frozen_string_literal: true

require_relative "util"

module Shop
  # An item.
  class Item < Base
    attr_reader :cost

    # The price.
    def price
      cost + helper
    end

    def self.build
      new
    end
  end
end

def test_price; end
