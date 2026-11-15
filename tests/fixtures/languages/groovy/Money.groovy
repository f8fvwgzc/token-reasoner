package shop

class Money {
    /** Renders cents as dollars. */
    static String formatMoney(int cents) {
        return String.format('$%d.%02d', cents.intdiv(100), cents % 100)
    }
}
