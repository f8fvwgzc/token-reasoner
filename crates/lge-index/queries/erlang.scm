; Erlang (the grammar crate does not export its tags query). A record is a
; struct; `m:f()` is a call of `f` on the module `m`.

(fun_decl clause: (function_clause name: (atom) @name)) @definition.function
(record_decl name: (atom) @name) @definition.class
; Typespecs name types: `amount()` in `-spec total([amount()]) -> amount().` is no call.
(spec fun: (atom) @name) @definition.skip
(type_alias name: (type_name name: (atom) @name)) @definition.skip
(opaque name: (type_name name: (atom) @name)) @definition.skip
(callback fun: (atom) @name) @definition.skip

(call expr: (atom) @name) @reference.call
