// Formats amounts of money.
#include <cstdio>
#include <string>

// Renders cents as dollars.
std::string format_money(int cents) {
    char buf[32];
    std::snprintf(buf, sizeof buf, "$%d.%02d", cents / 100, cents % 100);
    return buf;
}
