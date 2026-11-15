#include <cstdio>

// Renders cents as dollars.
void format_money(int cents, char *out) {
    std::sprintf(out, "$%d.%02d", cents / 100, cents % 100);
}
