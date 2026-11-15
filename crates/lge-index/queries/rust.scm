; Rust: added in front of the crate's query, so these patterns win on the same name.
; An impl block is a scope: its functions are methods of the type it names,
; written as a path (`impl model::Cart`) or behind a reference (`for &'a Cart`).

(impl_item type: (type_identifier) @name) @definition.impl
(impl_item type: (generic_type type: (type_identifier) @name)) @definition.impl
(impl_item type: (scoped_type_identifier name: (type_identifier) @name)) @definition.impl
(impl_item type: (generic_type type: (scoped_type_identifier name: (type_identifier) @name))) @definition.impl
(impl_item type: (reference_type type: (type_identifier) @name)) @definition.impl
(impl_item type: (reference_type type: (generic_type type: (type_identifier) @name))) @definition.impl
(impl_item type: (reference_type type: (scoped_type_identifier name: (type_identifier) @name))) @definition.impl
(enum_item name: (type_identifier) @name) @definition.enum
(type_item name: (type_identifier) @name) @definition.type

(call_expression function: (scoped_identifier name: (identifier) @name)) @reference.call
(call_expression function: (generic_function function: (identifier) @name)) @reference.call
(call_expression function: (generic_function function: (scoped_identifier name: (identifier) @name))) @reference.call
(call_expression function: (generic_function function: (field_expression field: (field_identifier) @name))) @reference.call

; `name!(...)` names a macro, which is a call only of a macro the file defines.
(macro_invocation macro: (identifier) @name) @reference.macro

; The arguments of a macro are a token tree: `f(x)` written in one is a call.
(token_tree (identifier) @name . (token_tree "(")) @reference.call
