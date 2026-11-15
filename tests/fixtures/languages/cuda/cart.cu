// Prices a shopping cart on the GPU.
#include <cuda_runtime.h>

void format_money(int cents, char *out);

// The prices of the items a customer picked.
struct Cart {
    const int *items;
    int count;

    // Adds up the item prices.
    __host__ int subtotal() const {
        int sum = 0;
        for (int i = 0; i < count; i++) {
            sum += items[i];
        }
        return sum;
    }

    // The subtotal with tax.
    __host__ int total() const;
};

// Adds the sales tax to every price.
__global__ void apply_tax_kernel(int *prices, int n) {
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < n) {
        prices[i] = apply_tax(prices[i]);
    }
}

// Adds the sales tax to an amount.
__host__ __device__ int apply_tax(int amount) {
    return amount * 108 / 100;
}

int Cart::total() const {
    return apply_tax(this->subtotal());
}

// Prices the cart and formats the result.
void checkout(const Cart &cart, char *out) {
    int total = cart.total();
    format_money(apply_tax(total), out);
}
