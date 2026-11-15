//! Formats amounts of money.

/// Renders cents as dollars.
pub fn format_money(cents: u64) -> String {
    format!("${}.{:02}", cents / 100, cents % 100)
}
