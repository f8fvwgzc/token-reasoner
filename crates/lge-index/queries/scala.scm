; Scala (the grammar crate has no tags query). An object is a class with one instance.

(class_definition name: (identifier) @name) @definition.class
(object_definition name: (identifier) @name) @definition.class
(trait_definition name: (identifier) @name) @definition.interface
(enum_definition name: (identifier) @name) @definition.enum
(function_definition name: (identifier) @name) @definition.function
(function_declaration name: (identifier) @name) @definition.method

(instance_expression "new" @name (template_body)) @definition.anonymous

(call_expression function: (identifier) @name) @reference.call
(instance_expression (type_identifier) @name) @reference.call
(call_expression function: (field_expression field: (identifier) @name)) @reference.call
(call_expression function: (generic_function function: (identifier) @name)) @reference.call
(call_expression function: (generic_function function: (field_expression field: (identifier) @name))) @reference.call
