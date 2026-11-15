<?php

/** Renders cents as dollars. */
function format_money(int $cents): string
{
    return sprintf('$%d.%02d', intdiv($cents, 100), $cents % 100);
}
