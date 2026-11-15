; Luau (the grammar crate has no tags query; its tree follows Lua's).

(function_declaration
  name: [
    (identifier) @name
    (dot_index_expression field: (identifier) @name)
  ]) @definition.function
(function_declaration
  name: (method_index_expression method: (identifier) @name)) @definition.method
; `local double = function(x) ... end` defines `double`.
(variable_declaration
  (assignment_statement
    (variable_list . name: (identifier) @name .)
    (expression_list . value: (function_definition) .))) @definition.function
(type_definition name: (identifier) @name) @definition.type

(function_call
  name: [
    (identifier) @name
    (dot_index_expression field: (identifier) @name)
    (method_index_expression method: (identifier) @name)
  ]) @reference.call
