package shop

import "testing"

func TestCheckout(t *testing.T) {
	cart := &Cart{items: []int{100, 250}}
	if got := Checkout(cart); got == "" {
		t.Errorf("empty checkout: %q", got)
	}
}
