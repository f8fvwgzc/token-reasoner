// Prices a shopping cart.
#include <numeric>
#include <string>
#include <vector>

std::string format_money(int cents);

namespace shop {

// The prices of the items a customer picked.
class Cart {
public:
    // Adds up the item prices.
    int subtotal() const {
        return std::accumulate(items_.begin(), items_.end(), 0);
    }

    // The subtotal with tax.
    int total() const;

private:
    std::vector<int> items_;
};

// Adds the sales tax to an amount.
int apply_tax(int amount) {
    return amount * 108 / 100;
}

int Cart::total() const {
    return apply_tax(this->subtotal());
}

// Prices the cart and formats the result.
std::string checkout(const Cart &cart) {
    int total = cart.total();
    return format_money(apply_tax(total));
}

}  // namespace shop
