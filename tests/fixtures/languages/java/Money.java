package shop;

/** Formats amounts of money. */
public final class Money {
    /** Renders cents as dollars. */
    static String formatMoney(int cents) {
        return String.format("$%d.%02d", cents / 100, cents % 100);
    }
}
