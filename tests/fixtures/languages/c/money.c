/* Formats amounts of money. */
#include <stdio.h>

/* Renders cents as dollars. */
void format_money(int cents, char *out) {
    sprintf(out, "$%d.%02d", cents / 100, cents % 100);
}
