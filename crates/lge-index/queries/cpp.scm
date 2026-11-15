; C++ (also used for CUDA): definitions with a body, classes, namespaces, calls.

; A googletest case `TEST(Suite, Name) { ... }` is the test `Name`, not a
; function called `TEST`.
((function_definition
  declarator: (function_declarator declarator: (identifier) @ignore))
  (#any-of? @ignore "TEST" "TEST_F" "TEST_P" "TYPED_TEST" "TYPED_TEST_P"))
((function_definition
  declarator: (function_declarator
    declarator: (identifier) @local.reference
    parameters: (parameter_list (parameter_declaration) . (parameter_declaration type: (type_identifier) @name))))
  @definition.test
  (#any-of? @local.reference "TEST" "TEST_F" "TEST_P" "TYPED_TEST" "TYPED_TEST_P"))

(function_definition
  declarator: [
    (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name)
    (function_declarator declarator: (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name))
    (pointer_declarator declarator: (function_declarator declarator: [(identifier) (field_identifier)] @name))
    (pointer_declarator declarator: (function_declarator declarator: (qualified_identifier name: (identifier) @name)))
    (reference_declarator (function_declarator declarator: [(identifier) (field_identifier)] @name))
    (reference_declarator (function_declarator declarator: (qualified_identifier name: (identifier) @name)))
  ]) @definition.function

; A method declared in its class and defined elsewhere: a symbol only when the
; file has no definition of it, and what a bare call in the class names.
(field_declaration_list
  (field_declaration declarator: (function_declarator declarator: (field_identifier) @name)) @definition.declaration)

(preproc_function_def name: (identifier) @name) @definition.macro
(class_specifier name: (type_identifier) @name body: (field_declaration_list)) @definition.class
(struct_specifier name: (type_identifier) @name body: (field_declaration_list)) @definition.class
(union_specifier name: (type_identifier) @name body: (field_declaration_list)) @definition.class
(enum_specifier name: (type_identifier) @name body: (enumerator_list)) @definition.enum
(namespace_definition name: (namespace_identifier) @name) @definition.module
(namespace_definition name: (nested_namespace_specifier (namespace_identifier) @name .)) @definition.module
(type_definition declarator: (type_identifier) @name) @definition.type
(alias_declaration name: (type_identifier) @name) @definition.type

; `class Child : public Base`: the bases a member may come from.
(base_class_clause [(type_identifier) (qualified_identifier) (template_type)] @name) @reference.base

(call_expression function: (identifier) @name) @reference.call
(call_expression function: (field_expression field: (field_identifier) @name)) @reference.call
(call_expression function: (qualified_identifier name: (identifier) @name)) @reference.call
(call_expression function: (qualified_identifier name: (qualified_identifier name: (identifier) @name))) @reference.call
(call_expression function: (qualified_identifier name: (qualified_identifier name: (qualified_identifier name: (identifier) @name)))) @reference.call
(call_expression function: (template_function name: (identifier) @name)) @reference.call
(call_expression function: (qualified_identifier name: (template_function name: (identifier) @name))) @reference.call
(new_expression type: (type_identifier) @name) @reference.call
(new_expression type: (qualified_identifier name: (type_identifier) @name)) @reference.new
