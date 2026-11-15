; Dart: the crate's query gives a function the range of its signature only, so
; its calls would fall outside it; this one tags the declaration with its body.

(class_declaration name: (identifier) @name) @definition.class
(mixin_declaration (identifier) @name) @definition.class
(extension_declaration name: (identifier) @name) @definition.impl
(enum_declaration name: (identifier) @name) @definition.enum
(method_declaration signature: (method_signature (function_signature name: (identifier) @name))) @definition.method
(method_declaration signature: (method_signature (getter_signature name: (identifier) @name))) @definition.method
; Constructors: `Cart(...)`, `Cart.empty()` (the name next to the parameters)
; and `factory Cart.of(...)`, with a body or without one.
(method_declaration
  signature: (method_signature
    [(constructor_signature (identifier) @name . (formal_parameter_list))
     (factory_constructor_signature (identifier) @name . (formal_parameter_list))])) @definition.method
(declaration
  [(constructor_signature (identifier) @name . (formal_parameter_list))
   (factory_constructor_signature (identifier) @name . (formal_parameter_list))]) @definition.method
(function_declaration signature: (function_signature name: (identifier) @name)) @definition.function

(call_expression function: (identifier) @name) @reference.call
(call_expression function: (member_expression property: (identifier) @name)) @reference.call
(call_expression function: (instantiation_expression function: (identifier) @name)) @reference.call
(call_expression
  function: (instantiation_expression function: (member_expression property: (identifier) @name))) @reference.call
; `sb..write(a)..write(b)` calls `write` on `sb` twice.
(cascade (cascade_section (cascade_call_expression property: (identifier) @name))) @reference.cascade
