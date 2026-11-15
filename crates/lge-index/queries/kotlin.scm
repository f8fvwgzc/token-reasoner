; Kotlin (the kotlin-ng grammar has no tags query).

(class_declaration "interface" name: (identifier) @name) @definition.interface
(class_declaration (modifiers (class_modifier "enum")) name: (identifier) @name) @definition.enum
(class_declaration name: (identifier) @name) @definition.class
(object_declaration name: (identifier) @name) @definition.class
(function_declaration name: (identifier) @name) @definition.function
; A property with a custom getter is code that runs: `val doubled get() = total() * 2`.
(property_declaration (variable_declaration (identifier) @name) (getter (function_body))) @definition.method

; An object expression is an anonymous class; a lambda may have a receiver.
(object_literal "object" @name) @definition.anonymous
(lambda_literal "{" @name) @definition.lambda

(call_expression . (identifier) @name) @reference.call
(call_expression . (navigation_expression (_) (identifier) @name .)) @reference.call
