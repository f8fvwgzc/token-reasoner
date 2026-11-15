import Foundation

/// Renders cents as dollars.
func formatMoney(_ cents: Int) -> String {
    return String(format: "$%d.%02d", cents / 100, cents % 100)
}
