; Haskell (the grammar crate has no tags query). A data type is a class, a type
; class an interface; an application `f x` calls `f`.

(data_type name: (name) @name) @definition.class
(newtype name: (name) @name) @definition.class
(type_synonym name: (name) @name) @definition.type
(class name: (name) @name) @definition.interface
; An instance implements the class methods for one type: its functions are that
; type's, not one function per name for the file.
(instance patterns: (type_patterns . (name) @name)) @definition.impl
(instance patterns: (type_patterns . (parens type: (apply constructor: (name) @name)))) @definition.impl
(function name: (variable) @name) @definition.function
(declarations (bind name: (variable) @name) @definition.function)

(apply function: (variable) @name) @reference.call
(apply function: (qualified id: (variable) @name)) @reference.call
; `f $ x` applies `f` like `f (x)`.
((infix left_operand: (variable) @name operator: (operator) @local.reference) @reference.call
  (#eq? @local.reference "$"))
((infix left_operand: (qualified id: (variable) @name) operator: (operator) @local.reference) @reference.call
  (#eq? @local.reference "$"))
