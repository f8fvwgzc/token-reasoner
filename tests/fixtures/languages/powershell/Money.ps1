# Renders cents as dollars.
function Format-Money([int] $Cents) {
    return '${0}.{1:D2}' -f [math]::Floor($Cents / 100), ($Cents % 100)
}
