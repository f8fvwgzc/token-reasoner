; Ruby: the crate's query with one change. A constant is a call only where it
; is called (`Integer("3")`): the crate's pattern made every constant a call,
; so `class Cart < Base`, `raise CartError` and `LIMIT` became call sites.

; Method definitions

(
  (comment)* @doc
  .
  [
    (method
      name: (_) @name) @definition.method
    (singleton_method
      name: (_) @name) @definition.method
  ]
  (#strip! @doc "^#\\s*")
  (#select-adjacent! @doc @definition.method)
)

(alias
  name: (_) @name) @definition.method

(setter
  (identifier) @ignore)

; Class definitions

(
  (comment)* @doc
  .
  [
    (class
      name: [
        (constant) @name
        (scope_resolution
          name: (_) @name)
      ]) @definition.class
    (singleton_class
      value: [
        (constant) @name
        (scope_resolution
          name: (_) @name)
      ]) @definition.class
  ]
  (#strip! @doc "^#\\s*")
  (#select-adjacent! @doc @definition.class)
)

; Module definitions

(
  (module
    name: [
      (constant) @name
      (scope_resolution
        name: (_) @name)
    ]) @definition.module
)

; Calls

(call method: [(identifier) (constant)] @name) @reference.call

(
  (identifier) @name @reference.call
  (#is-not? local)
  (#not-match? @name "^(lambda|load|require|require_relative|__FILE__|__LINE__)$")
)
