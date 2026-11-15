//! The rule ladder shared by calls and by the `inherits`, `implements` and
//! `reference` facts: the enclosing class, the same file, an import, and only
//! then the whole repository by name.
//!
//! The first three rungs follow what the source says and give `static` edges.
//! The last one matches by name alone: one candidate is `inferred`, several are
//! left unresolved, and none means the name lives outside the repository.

use crate::edges::Prov;
use crate::imports::{Imports, Slot};
use crate::symbols::{one, Index, Lang, Want};
use lge_core::{kind, SymbolRec};

pub(crate) enum Found<'a> {
    Symbol(&'a SymbolRec, Prov),
    /// Several candidates: stored as unresolved, never narrowed down to one.
    Ambiguous,
    /// No candidate in the repository.
    External,
}

pub(crate) struct Resolver<'r, 'a> {
    pub ix: &'r Index<'a>,
    pub imports: &'r Imports<'a>,
    /// Class id -> base names as written in its `extends` clause or base list.
    pub bases: &'r std::collections::HashMap<&'a str, Vec<&'a str>>,
}

impl<'a> Resolver<'_, 'a> {
    /// `self.name` or `this.name` inside `owner`: the member of the class that
    /// directly encloses `owner`. Outside a class `self` is an ordinary variable,
    /// so nothing is found and the caller falls back to a name match.
    pub fn own_member(&self, owner: &SymbolRec, name: &str, want: Want) -> Option<&'a SymbolRec> {
        if self.ix.lang(&owner.file_path) != Lang::Other {
            let (class, _) = owner.qualified_name.rsplit_once('.')?;
            self.ix.member(&owner.file_path, class, Want::Class)?;
            return self.ix.member(&owner.file_path, &format!("{class}.{name}"), want);
        }
        let scope = self.scope_of(owner, true)?;
        self.member_of(owner, &scope, name, want)
    }

    /// A bare `name()` in a language with an implicit `this`: the member of the
    /// type that encloses `owner`.
    pub fn implicit_member(&self, owner: &SymbolRec, name: &str, want: Want) -> Option<&'a SymbolRec> {
        let scope = self.scope_of(owner, false)?;
        self.member_of(owner, &scope, name, want)
    }

    /// A bare `name()` in a type nested in another (a Java inner class): the
    /// member of an enclosing type further out. Which one runs also depends on
    /// the bases of the inner type, which are not known, so it is a match by
    /// name, not what the source names.
    pub fn outer_member(&self, owner: &SymbolRec, name: &str, want: Want) -> Option<&'a SymbolRec> {
        let file = owner.file_path.as_str();
        let mut scope = self.scope_of(owner, false)?;
        while let Some((outer, _)) = scope.rsplit_once('.') {
            scope = outer.to_string();
            if self.ix.member(file, &scope, Want::Scope).is_some() {
                if let Some(m) = self.ix.member(file, &format!("{scope}.{name}"), want) {
                    return Some(m);
                }
            }
        }
        None
    }

    /// The qualified name of the type whose members `self` names inside `owner`,
    /// in a language read through a tags query: `owner` itself when it is a
    /// type (a call in a field initializer belongs to the class), else the type
    /// around it. A method with a written owner and no type of that name in its
    /// file (a Go method of a struct declared in another file, a C++ method
    /// defined out of line, a Lua `function Cart:total()`) has that owner as its
    /// scope. With `modules`, a module counts as the scope of its functions
    /// (Elixir `__MODULE__.f()`, Erlang `?MODULE:f()`).
    fn scope_of(&self, owner: &SymbolRec, modules: bool) -> Option<String> {
        let file = owner.file_path.as_str();
        let is_type = |k: &str| Want::Scope.admits(k) || (modules && k == kind::MODULE);
        if is_type(&owner.kind) {
            return Some(owner.qualified_name.clone());
        }
        let (scope, _) = owner.qualified_name.rsplit_once('.')?;
        let found = self.ix.in_scope(file, scope);
        if found.iter().any(|s| is_type(&s.kind)) {
            return Some(scope.to_string());
        }
        // A type alias of that name (Luau's `type Cart = {...}`, Go's `type
        // Cents int`) declares no members: the methods written for it are its own.
        let untyped = found.iter().all(|s| s.kind == kind::TYPE);
        (untyped && owner.kind == kind::METHOD).then(|| scope.to_string())
    }

    /// The member `name` of `scope`. Without a type symbol of that name in the
    /// file only a method counts: a function nested in `owner` is no member.
    fn member_of(&self, owner: &SymbolRec, scope: &str, name: &str, want: Want) -> Option<&'a SymbolRec> {
        let file = owner.file_path.as_str();
        let member = self.ix.member(file, &format!("{scope}.{name}"), want)?;
        let typed = !self.ix.in_scope(file, scope).is_empty();
        (typed || member.kind == kind::METHOD).then_some(member)
    }

    /// True when `owner` is a type or a member of one (a language read through
    /// a tags query).
    pub fn in_type(&self, owner: &SymbolRec) -> bool {
        self.scope_of(owner, false).is_some()
    }

    /// The bases of the type around `owner` that the source does not resolve
    /// (in another file, or not in the repository): members they define may
    /// shadow what the file defines under the same name.
    pub fn has_unknown_base(&self, owner: &SymbolRec) -> bool {
        let Some(scope) = self.scope_of(owner, false) else {
            return false;
        };
        let Some(class) = self.ix.member(&owner.file_path, &scope, Want::Scope) else {
            return false;
        };
        self.bases.get(class.id.as_str()).is_some_and(|bases| {
            bases
                .iter()
                .any(|b| self.bare(&class.file_path, b, Want::Base).is_none())
        })
    }

    /// A bare `name` used by `caller`: the one symbol of the file with that name
    /// that is in scope there, at the top level or in a scope around the caller
    /// (a function in a sibling module or namespace is not).
    pub fn visible(&self, caller: &SymbolRec, file: &str, name: &str, want: Want) -> Option<&'a SymbolRec> {
        let in_scope = |s: &&'a SymbolRec| {
            let scope = s.qualified_name.rsplit_once('.').map_or("", |(scope, _)| scope);
            caller.qualified_name == scope || caller.qualified_name.starts_with(&format!("{scope}."))
        };
        one(self.ix.in_file(file, name, want).into_iter().filter(in_scope).collect())
    }

    /// `self.name` or `this.name` where the member is defined on a base class.
    /// The chain of bases is followed only through bases the source resolves
    /// (same file or import). The result is the member when exactly one is
    /// found: with several bases defining it, which one wins is the language's
    /// resolution order, and that is not guessed here.
    pub fn inherited_member(&self, owner: &SymbolRec, name: &str, want: Want) -> Option<&'a SymbolRec> {
        let scope = match self.ix.lang(&owner.file_path) {
            Lang::Other => self.scope_of(owner, false)?,
            _ => owner.qualified_name.rsplit_once('.')?.0.to_string(),
        };
        let class = self.ix.member(&owner.file_path, &scope, Want::Class)?;
        let mut found: Vec<&'a SymbolRec> = Vec::new();
        self.members_of_bases(class, name, want, 8, &mut found);
        match found.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }

    fn members_of_bases(
        &self,
        class: &'a SymbolRec,
        name: &str,
        want: Want,
        depth: u8,
        found: &mut Vec<&'a SymbolRec>,
    ) {
        // The depth bound also ends a cycle of bases.
        if depth == 0 {
            return;
        }
        let Some(bases) = self.bases.get(class.id.as_str()) else {
            return;
        };
        for base_name in bases {
            let Some((base, _)) = self.bare(&class.file_path, base_name, Want::Class) else {
                continue;
            };
            let member = format!("{}.{}", base.qualified_name, name);
            match self.ix.member(&base.file_path, &member, want) {
                Some(m) if !found.iter().any(|f| f.id == m.id) => found.push(m),
                Some(_) => {}
                None => self.members_of_bases(base, name, want, depth - 1, found),
            }
        }
    }

    /// A bare `name` used in `file`: defined in that file, or bound by an import.
    pub fn bare(&self, file: &str, name: &str, want: Want) -> Option<(&'a SymbolRec, Prov)> {
        if let Some(s) = self.ix.one_in_file(file, name, want) {
            return Some((s, Prov::SAME_FILE));
        }
        match self.imports.name(file, name)? {
            Slot::One((target, original)) => self
                .imports
                .exported(self.ix, target, original, name, want)
                .map(|s| (s, Prov::IMPORTED)),
            Slot::Ambiguous => None,
        }
    }

    /// `receiver.name` used in `file`, where the receiver is an imported module
    /// (`util.slugify`) or names an imported or same-file class (`Repo().save`,
    /// `Repo.save`). A receiver that is bound by an import is resolved through
    /// that import or not at all.
    pub fn on_receiver(
        &self,
        file: &str,
        receiver: &str,
        name: &str,
        in_module: Want,
        in_class: Want,
    ) -> Option<(&'a SymbolRec, Prov)> {
        if let Some(bound) = self.imports.module(file, receiver) {
            let Slot::One(target) = bound else { return None };
            return self
                .imports
                .exported(self.ix, target, name, name, in_module)
                .map(|s| (s, Prov::IMPORTED));
        }
        let mut class_name = receiver.trim_end_matches("()");
        // PowerShell's `[Cart]::Round()` names the class `Cart`.
        if self.ix.lang(file) == Lang::Other {
            class_name = class_name
                .strip_prefix('[')
                .and_then(|c| c.strip_suffix(']'))
                .unwrap_or(class_name);
        }
        let class = match self.imports.name(file, class_name) {
            Some(Slot::One((target, original))) => {
                self.imports
                    .exported(self.ix, target, original, class_name, Want::Class)?
            }
            Some(Slot::Ambiguous) => return None,
            None => self.ix.one_in_file(file, class_name, Want::Class)?,
        };
        let member = format!("{}.{}", class.qualified_name, name);
        self.ix
            .member(&class.file_path, &member, in_class)
            .map(|s| (s, Prov::IMPORTED))
    }

    /// Repository-wide match by name alone, for a name used in `file`. `single`
    /// is the confidence band of a lone candidate. A symbol of a language that
    /// cannot call into `file`'s language by name (a Go function for a Ruby
    /// call) is no candidate; Python, TypeScript and JavaScript still see each other.
    pub fn anywhere(&self, file: &str, name: &str, want: Want, single: Prov) -> Found<'a> {
        let from = self.ix.family(file);
        let reachable = |s: &&'a SymbolRec| {
            let to = self.ix.family(&s.file_path);
            from.is_empty() || to.is_empty() || from == to
        };
        let mut found = self.ix.named_iter(name, want).filter(reachable);
        match (found.next(), found.next()) {
            (None, _) => Found::External,
            (Some(only), None) => Found::Symbol(only, single),
            _ => Found::Ambiguous,
        }
    }
}
