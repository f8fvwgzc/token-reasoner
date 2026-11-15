; Go: added in front of the crate's query. A struct is a class (its methods are
; attached by receiver type), an interface is an interface.

(type_spec name: (type_identifier) @name type: (struct_type)) @definition.class
(type_spec name: (type_identifier) @name type: (interface_type)) @definition.interface

; `Must[int](v)` with one type argument reads as a conversion to a generic type.
(type_conversion_expression type: (generic_type type: (type_identifier) @name)) @reference.call
(type_conversion_expression type: (generic_type type: (qualified_type name: (type_identifier) @name))) @reference.call
