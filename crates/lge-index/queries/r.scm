; R: added in front of the crate's query. `obj$method()` calls, and reference
; classes made with setRefClass("Name", methods = list(m = function() ...)).

(call function: (extract_operator rhs: (identifier) @name)) @reference.call

((call
  function: (identifier) @ignore
  arguments: (arguments . (argument value: (string (string_content) @name)))) @definition.class
  (#eq? @ignore "setRefClass"))

((call
  function: (identifier) @ignore
  arguments: (arguments
    (argument
      name: (identifier) @ignore
      value: (call
        arguments: (arguments
          (argument name: (identifier) @name value: (function_definition)) @definition.method)))))
  (#any-of? @ignore "setRefClass" "methods"))

; R6 classes: R6Class("Name", public = list(m = function() ...), private = ...),
; called as `R6::R6Class` or `R6Class`.
((call
  function: [(identifier) (namespace_operator rhs: (identifier))] @local.reference
  arguments: (arguments . (argument value: (string (string_content) @name)))) @definition.class
  (#match? @local.reference "(^|::)R6Class$"))

((call
  function: [(identifier) (namespace_operator rhs: (identifier))] @local.reference
  arguments: (arguments
    (argument
      value: (call
        arguments: (arguments
          (argument name: (identifier) @name value: (function_definition)) @definition.method)))))
  (#match? @local.reference "(^|::)R6Class$"))
