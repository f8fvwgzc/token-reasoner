package shop

object Money {
  /** Renders cents as dollars. */
  def formatMoney(cents: Int): String = "$%d.%02d".format(cents / 100, cents % 100)
}
