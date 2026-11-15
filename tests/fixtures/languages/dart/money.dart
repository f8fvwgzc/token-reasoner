/// Renders cents as dollars.
String formatMoney(int cents) {
  final rest = (cents % 100).toString().padLeft(2, '0');
  return '\$${cents ~/ 100}.$rest';
}
