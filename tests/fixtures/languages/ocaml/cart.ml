(* Prices a shopping cart. *)

(* The prices of the items a customer picked. *)
type cart = { items : int list }

(* Adds the sales tax to an amount. *)
let apply_tax amount = amount * 108 / 100 (* NOTE: integer division keeps whole cents *)

module Cart = struct
  (* Adds up the item prices. *)
  let subtotal cart = List.fold_left ( + ) 0 cart.items

  (* The subtotal with tax. *)
  let total cart = apply_tax (subtotal cart)
end

(* Prices the cart and formats the result. *)
let checkout cart =
  let total = Cart.total cart in
  Money.format_money (apply_tax total)
