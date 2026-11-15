; Elm: the crate's query names calls `reference.function` together with type
; annotations and exposing lists, so this one replaces it.

(value_declaration (function_declaration_left . (lower_case_identifier) @name)) @definition.function
(type_alias_declaration name: (upper_case_identifier) @name) @definition.class
(type_declaration name: (upper_case_identifier) @name) @definition.type

(function_call_expr target: (value_expr name: (value_qid (lower_case_identifier) @name .))) @reference.call
; Pipelines apply a function as surely as a call: `x |> f` and `f <| x`.
((bin_op_expr
  (operator (operator_identifier) @local.reference)
  . (value_expr name: (value_qid (lower_case_identifier) @name .))) @reference.call
  (#eq? @local.reference "|>"))
((bin_op_expr
  (value_expr name: (value_qid (lower_case_identifier) @name .))
  . (operator (operator_identifier) @local.reference)) @reference.call
  (#eq? @local.reference "<|"))
