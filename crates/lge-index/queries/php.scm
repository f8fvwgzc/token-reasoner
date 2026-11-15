; PHP: added in front of the crate's query, whose call pattern misses a plain `f()`.

(function_call_expression function: (name) @name) @reference.call
(object_creation_expression (name) @name) @reference.call
(object_creation_expression (qualified_name (name) @name .)) @reference.call
(nullsafe_member_call_expression name: (name) @name) @reference.call
(enum_declaration name: (name) @name) @definition.enum
; A trait holds code, not signatures: the crate tags it as an interface.
(trait_declaration name: (name) @name) @definition.class
