; Objective-C (the grammar crate has no tags query). The @interface names the
; class; the @implementation is its scope; a message `[cart total]` is a call.

(class_interface . (identifier) @name) @definition.class
(class_implementation . (identifier) @name) @definition.impl
(protocol_declaration . (identifier) @name) @definition.interface
; A selector of several parts (`addItem:quantity:`) is one method named by its first part.
(method_definition (method_type) . (identifier) @name) @definition.method
(method_definition . (identifier) @name) @definition.method
(function_definition
  declarator: [
    (function_declarator declarator: (identifier) @name)
    (pointer_declarator declarator: (function_declarator declarator: (identifier) @name))
  ]) @definition.function
(struct_specifier name: (type_identifier) @name body: (field_declaration_list)) @definition.class

(call_expression function: (identifier) @name) @reference.call
; One message per send, named by the first part of its selector.
(message_expression receiver: (_) . method: (identifier) @name) @reference.call
