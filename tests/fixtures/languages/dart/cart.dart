import 'money.dart';

/// The prices of the items a customer picked.
class Cart {
  final List<int> items;

  Cart(this.items);

  /// Adds up the item prices.
  int subtotal() {
    return items.fold(0, (sum, price) => sum + price);
  }

  /// The subtotal with tax.
  int total() {
    return applyTax(this.subtotal());
  }
}

/// Adds the sales tax to an amount.
int applyTax(int amount) {
  return amount * 108 ~/ 100;
}

/// Prices the cart and formats the result.
String checkout(Cart cart) {
  final total = cart.total();
  return formatMoney(applyTax(total));
}
