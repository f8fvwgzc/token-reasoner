; Clojure (the grammar crate has no tags query). Every list form calls its head;
; the defining forms name what they define instead.

((list_lit . (sym_lit name: (sym_name) @ignore))
  (#any-of? @ignore "ns" "def" "defn" "defn-" "defmacro" "defmulti" "defmethod" "defrecord" "deftype" "defprotocol" "definterface" "defonce"))

((list_lit . (sym_lit name: (sym_name) @local.reference) . (sym_lit name: (sym_name) @name)) @definition.function
  (#any-of? @local.reference "defn" "defn-" "defmacro" "defmulti"))
((list_lit . (sym_lit name: (sym_name) @local.reference) . (sym_lit name: (sym_name) @name)) @definition.class
  (#any-of? @local.reference "defrecord" "deftype"))
((list_lit . (sym_lit name: (sym_name) @local.reference) . (sym_lit name: (sym_name) @name)) @definition.interface
  (#any-of? @local.reference "defprotocol" "definterface"))

(list_lit . (sym_lit name: (sym_name) @name)) @reference.call
