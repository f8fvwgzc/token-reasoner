; Solidity: added in front of the crate's query. A library has function bodies,
; so it is a class, not an interface; an event is not a definition lge keeps.

(library_declaration name: (identifier) @name) @definition.class
(enum_declaration name: (identifier) @name) @definition.enum
(event_definition name: (identifier) @name) @definition.event
(modifier_definition name: (identifier) @name) @definition.function
; The crate's method pattern covers contract bodies only.
(library_declaration (contract_body (function_definition name: (identifier) @name) @definition.method))
(interface_declaration (contract_body (function_definition name: (identifier) @name) @definition.method))
(constructor_definition "constructor" @name) @definition.method

; `function withdraw() onlyOwner` runs the modifier.
(modifier_invocation (identifier) @name) @reference.call
; `contract Token is Base`: the bases a member may come from.
(inheritance_specifier ancestor: (user_defined_type (identifier) @name .)) @reference.base
