; C#: the crate's own query has an invalid capture, so this one replaces it.

(namespace_declaration name: [(identifier) (qualified_name)] @name) @definition.module
; `namespace Shop.Billing;` scopes the rest of the file like a block namespace.
(file_scoped_namespace_declaration name: [(identifier) (qualified_name)] @name) @definition.file_namespace
(class_declaration name: (identifier) @name) @definition.class
(struct_declaration name: (identifier) @name) @definition.class
(record_declaration name: (identifier) @name) @definition.class
(interface_declaration name: (identifier) @name) @definition.interface
(enum_declaration name: (identifier) @name) @definition.enum
(method_declaration name: (identifier) @name) @definition.method
(constructor_declaration name: (identifier) @name) @definition.method
(local_function_statement name: (identifier) @name) @definition.function
; A property with a body is code that runs (`Total { get { ... } }`, `Doubled => ...`).
(property_declaration name: (identifier) @name value: (arrow_expression_clause)) @definition.method
(property_declaration name: (identifier) @name accessors: (accessor_list (accessor_declaration body: (_)))) @definition.method

(invocation_expression function: (identifier) @name) @reference.call
(invocation_expression function: (member_access_expression name: (identifier) @name)) @reference.call
(invocation_expression function: (generic_name (identifier) @name)) @reference.call
(invocation_expression function: (member_access_expression name: (generic_name (identifier) @name))) @reference.call
(object_creation_expression type: (identifier) @name) @reference.call
