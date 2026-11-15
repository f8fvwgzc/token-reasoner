; Groovy (the grammar crate has no tags query; its tree follows Java's).

(class_declaration name: (identifier) @name) @definition.class
(interface_declaration name: (identifier) @name) @definition.interface
(enum_declaration name: (identifier) @name) @definition.enum
(method_declaration name: (identifier) @name) @definition.method
; A Spock feature `def "totals the items"()` reads as a constructor called
; `def`; the indexer names it by its string.
(constructor_declaration name: (identifier) @name) @definition.method
; A script method `def checkout(Cart cart) { ... }` at the top level.
(function_definition name: (identifier) @name) @definition.function

(method_invocation name: (identifier) @name) @reference.call
(object_creation_expression type: (type_identifier) @name) @reference.call
