namespace Shop
{
    public static class Money
    {
        /// <summary>Renders cents as dollars.</summary>
        public static string FormatMoney(int cents)
        {
            return string.Format("${0}.{1:00}", cents / 100, cents % 100);
        }
    }
}
