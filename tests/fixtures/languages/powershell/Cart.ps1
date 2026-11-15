# Prices a shopping cart.
. "$PSScriptRoot/Money.ps1"

# The prices of the items a customer picked.
class Cart {
    [int[]] $Items

    # Adds up the item prices.
    [int] Subtotal() {
        return ($this.Items | Measure-Object -Sum).Sum
    }

    # The subtotal with tax.
    [int] Total() {
        return Get-Tax $this.Subtotal()
    }
}

# Adds the sales tax to an amount.
function Get-Tax([int] $Amount) {
    return [math]::Floor($Amount * 108 / 100)
}

# Prices the cart and formats the result.
function Invoke-Checkout([Cart] $Cart) {
    $total = $Cart.Total()
    return Format-Money (Get-Tax $total)
}
