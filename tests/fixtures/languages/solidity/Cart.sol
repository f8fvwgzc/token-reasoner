// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "./Money.sol";

/// The prices of the items a customer picked.
contract Cart {
    uint256[] private items;

    /// Adds up the item prices.
    function subtotal() public view returns (uint256 sum) {
        for (uint256 i = 0; i < items.length; i++) {
            sum += items[i];
        }
    }

    /// The subtotal with tax.
    function total() public view returns (uint256) {
        return applyTax(this.subtotal());
    }
}

/// Adds the sales tax to an amount.
function applyTax(uint256 amount) pure returns (uint256) {
    return amount * 108 / 100;
}

/// Prices the cart and formats the result.
function checkout(Cart cart) view returns (string memory) {
    uint256 total = cart.total();
    return formatMoney(applyTax(total));
}
