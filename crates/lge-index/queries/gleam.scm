; Gleam: the crate's query names calls `reference.function` together with
; record field reads, so this one replaces it.

(function name: (identifier) @name) @definition.function
(external_function name: (identifier) @name) @definition.function
(type_definition (type_name name: (type_identifier) @name)) @definition.class
(type_alias (type_name name: (type_identifier) @name)) @definition.type

(function_call function: (identifier) @name) @reference.call
(function_call function: (field_access field: (label) @name)) @reference.call
; `x |> f` applies `f`.
(binary_expression operator: "|>" right: (identifier) @name) @reference.call
(binary_expression operator: "|>" right: (field_access field: (label) @name)) @reference.call
