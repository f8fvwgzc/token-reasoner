; C: function definitions (never prototypes), function-like macros, structs,
; unions, enums, typedefs, calls.

(function_definition
  declarator: [
    (function_declarator declarator: (identifier) @name)
    (pointer_declarator declarator: (function_declarator declarator: (identifier) @name))
    (pointer_declarator declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name)))
  ]) @definition.function

(preproc_function_def name: (identifier) @name) @definition.macro
(struct_specifier name: (type_identifier) @name body: (field_declaration_list)) @definition.class
(union_specifier name: (type_identifier) @name body: (field_declaration_list)) @definition.class
(enum_specifier name: (type_identifier) @name body: (enumerator_list)) @definition.enum
(type_definition declarator: (type_identifier) @name) @definition.type

(call_expression function: (identifier) @name) @reference.call
(call_expression function: (field_expression field: (field_identifier) @name)) @reference.call
