; Perl (the grammar crate has no tags query). `Shop::Money::fmt(...)` is one
; token; the indexer splits it into the call `fmt` on `Shop::Money`.

(subroutine_declaration_statement name: (bareword) @name) @definition.function

(function_call_expression function: (function) @name) @reference.call
(ambiguous_function_call_expression function: (function) @name) @reference.call
(method_call_expression method: (method) @name) @reference.call
