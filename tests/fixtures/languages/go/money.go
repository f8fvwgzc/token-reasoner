package shop

import "fmt"

// FormatMoney renders cents as dollars.
func FormatMoney(cents int) string {
	return fmt.Sprintf("$%d.%02d", cents/100, cents%100)
}
