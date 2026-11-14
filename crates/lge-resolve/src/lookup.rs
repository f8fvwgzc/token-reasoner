//! The rule ladder shared by calls and by the `inherits`, `implements` and
//! `reference` facts: the enclosing class, the same file, an import, and only
//! then the whole repository by name.
//!
//! The first three rungs follow what the source says and give `static` edges.
//! The last one matches by name alone: one candidate is `inferred`, several are
//! left unresolved, and none means the name lives outside the repository.

use crate::edges::Prov;
use crate::imports::{Imports, Slot};
use crate::symbols::{Index, Want};
use lge_core::SymbolRec;

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
}

impl<'a> Resolver<'_, 'a> {
    /// `self.name` or `this.name` inside `owner`: the member of the class that
    /// directly encloses `owner`. Outside a class `self` is an ordinary variable,
    /// so nothing is found and the caller falls back to a name match.
    pub fn own_member(&self, owner: &SymbolRec, name: &str, want: Want) -> Option<&'a SymbolRec> {
        let (class, _) = owner.qualified_name.rsplit_once('.')?;
        self.ix.member(&owner.file_path, class, Want::Class)?;
        self.ix.member(&owner.file_path, &format!("{class}.{name}"), want)
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
        let class_name = receiver.trim_end_matches("()");
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

    /// Repository-wide match by name alone. `single` is the confidence band of a
    /// lone candidate.
    pub fn anywhere(&self, name: &str, want: Want, single: Prov) -> Found<'a> {
        match self.ix.named(name, want).as_slice() {
            [] => Found::External,
            [only] => Found::Symbol(only, single),
            _ => Found::Ambiguous,
        }
    }
}
