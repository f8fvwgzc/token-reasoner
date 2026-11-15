import gleam/int
import gleam/string

/// Renders cents as dollars.
pub fn format_money(cents: Int) -> String {
  "$" <> int.to_string(cents / 100) <> "." <> string.pad_start(int.to_string(cents % 100), 2, "0")
}
