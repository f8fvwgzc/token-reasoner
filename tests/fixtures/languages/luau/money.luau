-- Formats amounts of money.
local M = {}

-- Renders cents as dollars.
function M.formatMoney(cents: number): string
	return string.format("$%d.%02d", cents // 100, cents % 100)
end

return M
