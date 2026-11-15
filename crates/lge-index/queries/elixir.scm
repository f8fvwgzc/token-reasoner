; Elixir: added in front of the crate's query. A typespec names types: what
; reads as a call in `@spec total([integer]) :: integer` is no call.

((unary_operator
  operator: "@"
  operand: (call target: (identifier) @name)) @definition.skip
  (#any-of? @name "spec" "type" "typep" "opaque" "callback" "macrocallback"))
