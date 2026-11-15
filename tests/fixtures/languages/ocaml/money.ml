(* Formats amounts of money. *)

(* Renders cents as dollars. *)
let format_money cents = Printf.sprintf "$%d.%02d" (cents / 100) (cents mod 100)
