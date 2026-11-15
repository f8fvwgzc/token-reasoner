; Julia (the grammar crate has no tags query). The definition patterns come
; first, so the call written in a signature is not also a call site.

(module_definition name: (identifier) @name) @definition.module
(struct_definition (type_head (identifier) @name)) @definition.class
(struct_definition (type_head (binary_expression . (identifier) @name))) @definition.class
(struct_definition (type_head (parametrized_type_expression . (identifier) @name))) @definition.class
(struct_definition (type_head (binary_expression . (parametrized_type_expression . (identifier) @name)))) @definition.class
(abstract_definition (type_head (identifier) @name)) @definition.interface
(abstract_definition (type_head (binary_expression . (identifier) @name))) @definition.interface
(function_definition (signature (call_expression . (identifier) @name))) @definition.function
(function_definition (signature (call_expression . (field_expression (identifier) @name .)))) @definition.function
(function_definition (signature (typed_expression . (call_expression . (identifier) @name)))) @definition.function
(function_definition (signature (where_expression . (call_expression . (identifier) @name)))) @definition.function
(function_definition (signature (where_expression . (typed_expression . (call_expression . (identifier) @name))))) @definition.function
(assignment . (call_expression . (identifier) @name)) @definition.function
; `f(x) where {T} = ...`, `f(x)::Int = ...` and both.
(assignment . (where_expression . (call_expression . (identifier) @name))) @definition.function
(assignment . (typed_expression . (call_expression . (identifier) @name))) @definition.function
(assignment . (where_expression . (typed_expression . (call_expression . (identifier) @name)))) @definition.function
(macro_definition (signature (call_expression . (identifier) @name))) @definition.function

(call_expression . (identifier) @name) @reference.call
(call_expression . (field_expression (identifier) @name .)) @reference.call
; `f.(xs)` broadcasts `f`.
(broadcast_call_expression . (identifier) @name) @reference.call
(broadcast_call_expression . (field_expression (identifier) @name .)) @reference.call
