; Java: added in front of the crate's query. `new Cart()` calls the class.

(constructor_declaration name: (identifier) @name) @definition.method
(enum_declaration name: (identifier) @name) @definition.enum
(record_declaration name: (identifier) @name) @definition.class
(object_creation_expression type: (type_identifier) @name) @reference.call

; The body of an anonymous class: its methods are methods, not functions of the file.
(object_creation_expression "new" @name (class_body)) @definition.anonymous
