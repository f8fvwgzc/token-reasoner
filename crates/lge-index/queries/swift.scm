; Swift: the crate's query tags a method with the range of its whole class and
; has no calls, so this one replaces it. An extension is a scope of its type.

(class_declaration declaration_kind: "enum" name: (type_identifier) @name) @definition.enum
(class_declaration name: (type_identifier) @name) @definition.class
(class_declaration name: (user_type (type_identifier) @name)) @definition.impl
(protocol_declaration name: (type_identifier) @name) @definition.interface
(function_declaration name: (simple_identifier) @name) @definition.function
(protocol_function_declaration name: (simple_identifier) @name) @definition.method
(init_declaration "init" @name) @definition.method
; A computed property is code that runs (SwiftUI's `var body: some View { ... }`).
(property_declaration
  name: (pattern bound_identifier: (simple_identifier) @name)
  computed_value: (computed_property)) @definition.method

; `xs[0]` is a subscript: only arguments in parentheses or a trailing closure make a call.
(call_expression
  . (simple_identifier) @name
  (call_suffix [(value_arguments "(") (lambda_literal)])) @reference.call
(call_expression
  . (navigation_expression suffix: (navigation_suffix suffix: (simple_identifier) @name))
  (call_suffix [(value_arguments "(") (lambda_literal)])) @reference.call
