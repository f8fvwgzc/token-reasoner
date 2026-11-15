// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "@openzeppelin/contracts/utils/Strings.sol";

/// Renders cents as dollars.
function formatMoney(uint256 cents) pure returns (string memory) {
    return string.concat("$", Strings.toString(cents / 100));
}
