using System.Collections.Generic;
using System.Linq;

namespace Shop
{
    /// <summary>The prices of the items a customer picked.</summary>
    public class Cart
    {
        private readonly List<int> items = new List<int>();

        /// <summary>Adds up the item prices.</summary>
        public int Subtotal()
        {
            return items.Sum();
        }

        /// <summary>The subtotal with tax.</summary>
        public int Total()
        {
            return Checkout.ApplyTax(this.Subtotal());
        }
    }

    public interface IPricer
    {
        int Total();
    }

    public static class Checkout
    {
        /// <summary>Adds the sales tax to an amount.</summary>
        public static int ApplyTax(int amount)
        {
            return amount * 108 / 100;
        }

        /// <summary>Prices the cart and formats the result.</summary>
        public static string Run(Cart cart)
        {
            var total = cart.Total();
            return Money.FormatMoney(ApplyTax(total));
        }
    }
}
