// Package shop prices a shopping cart.
package shop

// Cart holds the prices of the items a customer picked.
type Cart struct {
	items []int
}

// Pricer is anything that has a total.
type Pricer interface {
	Total() int
}

// Subtotal adds up the item prices.
func (c *Cart) Subtotal() int {
	sum := 0
	for _, p := range c.items {
		sum += p
	}
	return sum
}

// Total is the subtotal with tax.
func (c *Cart) Total() int {
	return ApplyTax(c.Subtotal())
}

// ApplyTax adds the sales tax to an amount.
func ApplyTax(amount int) int {
	return amount * 108 / 100 // HACK: the rate is fixed until tax tables exist
}

// Checkout prices the cart and formats the result.
func Checkout(cart *Cart) string {
	total := cart.Total()
	return FormatMoney(ApplyTax(total))
}
