; Nix (the grammar crate does not export its tags query). A binding to a lambda
; is a function; an application `f x` calls `f`.

(binding attrpath: (attrpath attr: (identifier) @name .) expression: (function_expression)) @definition.function

(apply_expression function: (variable_expression name: (identifier) @name)) @reference.call
(apply_expression function: (select_expression attrpath: (attrpath attr: (identifier) @name .))) @reference.call
