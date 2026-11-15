package shop

/** Renders cents as dollars. */
fun formatMoney(cents: Int): String = "$%d.%02d".format(cents / 100, cents % 100)
