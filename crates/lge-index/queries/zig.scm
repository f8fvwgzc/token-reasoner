; Zig (the grammar crate has no tags query). `const Cart = struct { ... }` is a class.

(variable_declaration (identifier) @name (struct_declaration)) @definition.class
(variable_declaration (identifier) @name (union_declaration)) @definition.class
(variable_declaration (identifier) @name (enum_declaration)) @definition.enum
; A generic type is a function that returns `struct { ... }`: its functions are methods.
(function_declaration
  name: (identifier) @name
  body: (block (expression_statement (return_expression (struct_declaration))))) @definition.class
(function_declaration name: (identifier) @name) @definition.function
(test_declaration [(string (string_content) @name) (identifier) @name]) @definition.test

(call_expression function: (identifier) @name) @reference.call
(call_expression function: (field_expression member: (identifier) @name)) @reference.call
