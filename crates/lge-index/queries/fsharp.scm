; F#: the crate's query gives a function the range of its whole module, so this
; one replaces it.

(named_module name: (long_identifier) @name) @definition.module
(module_defn (identifier) @name) @definition.module
(function_or_value_defn (function_declaration_left . (identifier) @name)) @definition.function
(type_definition (anon_type_defn (type_name type_name: (identifier) @name))) @definition.class
(type_definition (record_type_defn (type_name type_name: (identifier) @name))) @definition.class
(type_definition (union_type_defn (type_name type_name: (identifier) @name))) @definition.type
(type_definition (interface_type_defn (type_name type_name: (identifier) @name))) @definition.interface
(member_defn (method_or_prop_defn name: (property_or_ident method: (identifier) @name))) @definition.method
; `static member Empty() = ...` names no instance.
(member_defn (method_or_prop_defn name: (property_or_ident . (identifier) @name .))) @definition.method

(application_expression . (long_identifier_or_op (identifier) @name)) @reference.call
(application_expression . (long_identifier_or_op (long_identifier (identifier) @name .))) @reference.call
; Pipelines apply a function as surely as a call: `x |> f` and `f <| x`.
((infix_expression
  (infix_op) @local.reference
  . (long_identifier_or_op [(identifier) @name (long_identifier (identifier) @name .)])) @reference.call
  (#eq? @local.reference "|>"))
((infix_expression
  (long_identifier_or_op [(identifier) @name (long_identifier (identifier) @name .)])
  . (infix_op) @local.reference) @reference.call
  (#eq? @local.reference "<|"))
