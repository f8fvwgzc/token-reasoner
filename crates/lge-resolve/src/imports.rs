//! Import resolution: which file an import names, and what each local name it
//! binds stands for. Python and TypeScript/JavaScript have their own rules; the
//! language of the importing file decides which apply.

use crate::edges::{edge, Evidence, Output, Prov};
use crate::symbols::{one, Index, Lang, Want, SCRIPT_EXTENSIONS};
use lge_core::{fact, EdgeType, FileRec, ImportSite, ResolveInput, SymbolRec};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::Hash;

/// What a local name is bound to.
///
/// When two imports in one file bind the same name to different things (a
/// `try`/`except ImportError` pair, a conditional import), nothing says which is
/// in effect at a given call. That is several candidates, so the name is
/// `Ambiguous` and no static rule resolves through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Slot<T> {
    One(T),
    Ambiguous,
}

fn bind<K: Hash + Eq, V: PartialEq>(map: &mut HashMap<K, Slot<V>>, key: K, value: V) {
    match map.entry(key) {
        Entry::Vacant(e) => {
            e.insert(Slot::One(value));
        }
        Entry::Occupied(mut e) => {
            if *e.get() != Slot::One(value) {
                e.insert(Slot::Ambiguous);
            }
        }
    }
}

pub(crate) struct Imports<'a> {
    /// (file, local name) -> (target file, name in the target file).
    names: HashMap<(&'a str, &'a str), Slot<(&'a str, &'a str)>>,
    /// (file, local name) -> target file, for a name that stands for a whole module.
    modules: HashMap<(&'a str, &'a str), Slot<&'a str>>,
    /// file -> the files it re-exports wholesale (`export * from "./x"`).
    stars: HashMap<&'a str, BTreeSet<&'a str>>,
    /// file -> the symbol ids the indexer marked as its default export. An id
    /// that is no symbol of the input (an anonymous function, an object) stays
    /// in the set: the file has a default export, and it is not some other symbol.
    defaults: HashMap<&'a str, BTreeSet<&'a str>>,
}

impl<'a> Imports<'a> {
    /// Resolve every import site: record what it binds, add the `IMPORTS` edge,
    /// and count the imports that leave the repository.
    pub fn resolve(ix: &Index<'a>, input: &'a ResolveInput, out: &mut Output) -> Imports<'a> {
        let mut imports = Imports {
            names: HashMap::new(),
            modules: HashMap::new(),
            stars: HashMap::new(),
            defaults: HashMap::new(),
        };
        for f in input.facts.iter().filter(|f| f.kind == fact::DEFAULT_EXPORT) {
            let file = match ix.sym(&f.symbol_id) {
                Some(s) => s.file_path.as_str(),
                None => f.file_path.as_str(),
            };
            imports.defaults.entry(file).or_default().insert(&f.symbol_id);
        }
        let python = PythonModules::new(ix);
        for site in &input.imports {
            let Some(file) = ix.file(&site.file_path) else { continue };
            let target = match Lang::of(file) {
                Lang::Python => imports.bind_python(ix, &python, file, site),
                Lang::Script => imports.bind_script(ix, file, site),
                Lang::Other => continue,
            };
            match target {
                // The import site carries no line, so the evidence is the file alone.
                Some(t) if t.path != file.path => out.add_edge(edge(
                    &file.id,
                    &t.id,
                    EdgeType::Imports,
                    Prov::IMPORTED,
                    Evidence::line(&file.path, 0),
                )),
                Some(_) => {}
                None => out.stats.imports_external += 1,
            }
        }
        imports
    }

    /// `from m import name` binds a submodule when `m.name` is a module, otherwise
    /// a name defined in `m`; `import m` binds the module.
    fn bind_python(
        &mut self,
        ix: &Index<'a>,
        python: &PythonModules<'a>,
        file: &'a FileRec,
        site: &'a ImportSite,
    ) -> Option<&'a FileRec> {
        let key = (file.path.as_str(), site.alias.as_str());
        if site.name.is_empty() {
            let target = python.find(ix, &file.path, &site.module)?;
            bind(&mut self.modules, key, target.path.as_str());
            return Some(target);
        }
        let sub = if site.module.ends_with('.') {
            format!("{}{}", site.module, site.name)
        } else {
            format!("{}.{}", site.module, site.name)
        };
        if let Some(target) = python.find(ix, &file.path, &sub) {
            bind(&mut self.modules, key, target.path.as_str());
            return Some(target);
        }
        let target = python.find(ix, &file.path, &site.module)?;
        bind(&mut self.names, key, (target.path.as_str(), site.name.as_str()));
        Some(target)
    }

    /// `name` is the exported name, `default`, or `*` for a namespace import or
    /// `require`. A `*` that binds no local name is `export * from`.
    fn bind_script(&mut self, ix: &Index<'a>, file: &'a FileRec, site: &'a ImportSite) -> Option<&'a FileRec> {
        let target = script_module(ix, &file.path, &site.module)?;
        let key = (file.path.as_str(), site.alias.as_str());
        match site.name.as_str() {
            // `import "./setup"`: a dependency that binds nothing.
            "" => {}
            "*" if site.alias.is_empty() || site.alias == "*" => {
                self.stars.entry(&file.path).or_default().insert(&target.path);
            }
            "*" => bind(&mut self.modules, key, target.path.as_str()),
            name => bind(&mut self.names, key, (target.path.as_str(), name)),
        }
        Some(target)
    }

    /// What the local `alias` in `file` stands for when an import bound it to a name.
    pub fn name(&self, file: &str, alias: &str) -> Option<Slot<(&'a str, &'a str)>> {
        self.names.get(&(file, alias)).copied()
    }

    /// The file the local `alias` in `file` stands for when an import bound it to a module.
    pub fn module(&self, file: &str, alias: &str) -> Option<Slot<&'a str>> {
        self.modules.get(&(file, alias)).copied()
    }

    /// The symbol that `name`, taken from `file` and bound locally as `alias`, stands for.
    pub fn exported(&self, ix: &Index<'a>, file: &str, name: &str, alias: &str, want: Want) -> Option<&'a SymbolRec> {
        match ix.lang(file) {
            Lang::Script => self.script_export(ix, file, name, alias, want, true),
            _ => ix.one_in_file(file, name, want),
        }
    }

    /// TypeScript and JavaScript exports. `default` is the symbol the indexer
    /// marked; only when it marked none is the symbol with the alias's name
    /// taken. A name the file does not define but imports under that same name is
    /// a re-export and is followed, one step only.
    fn script_export(
        &self,
        ix: &Index<'a>,
        file: &str,
        name: &str,
        alias: &str,
        want: Want,
        follow: bool,
    ) -> Option<&'a SymbolRec> {
        if name == "default" {
            if let Some(marked) = self.defaults.get(file) {
                let id = one(marked.iter().copied().collect())?;
                return ix.sym(id).filter(|s| want.admits(&s.kind));
            }
        } else {
            // A name the file defines is that definition, whatever its kind: a
            // re-export of the same name from elsewhere would be shadowed by it.
            let here = ix.in_file(file, name, Want::Nameable);
            if !here.is_empty() {
                return one(here.into_iter().filter(|s| want.admits(&s.kind)).collect());
            }
        }
        if follow {
            match self.name(file, name) {
                Some(Slot::One((target, original))) => {
                    return self.script_export(ix, target, original, alias, want, false);
                }
                Some(Slot::Ambiguous) => return None,
                None => {}
            }
        }
        if name == "default" {
            return ix.one_in_file(file, alias, want);
        }
        if !follow {
            return None;
        }
        let mut found: BTreeMap<&str, &'a SymbolRec> = BTreeMap::new();
        for target in self.stars.get(file).into_iter().flatten() {
            if let Some(s) = self.script_export(ix, target, name, alias, want, false) {
                found.insert(&s.id, s);
            }
        }
        one(found.into_values().collect())
    }
}

/// Dotted module suffix -> Python files. `svc/app/tasks.py` answers to
/// `svc.app.tasks`, `app.tasks` and `tasks`.
struct PythonModules<'a> {
    by_suffix: HashMap<String, Vec<&'a FileRec>>,
}

impl<'a> PythonModules<'a> {
    fn new(ix: &Index<'a>) -> Self {
        let mut by_suffix: HashMap<String, Vec<&'a FileRec>> = HashMap::new();
        for file in ix.files().filter(|f| Lang::of(f) == Lang::Python) {
            let trimmed = file.path.trim_end_matches(".py").trim_end_matches("/__init__");
            let parts: Vec<&str> = trimmed.split('/').collect();
            for i in 0..parts.len() {
                by_suffix.entry(parts[i..].join(".")).or_default().push(file);
            }
        }
        PythonModules { by_suffix }
    }

    /// The file a module name stands for, seen from `importer`. A relative
    /// import is a path from the importer's package; one with more dots than
    /// the importer has directories leaves the indexed root and names no file.
    /// An absolute import is looked up by suffix; when several files answer,
    /// only one in the importer's own top-level directory is taken, and
    /// otherwise none.
    fn find(&self, ix: &Index<'a>, importer: &str, module: &str) -> Option<&'a FileRec> {
        if module.is_empty() {
            return None;
        }
        if module.starts_with('.') {
            let dots = module.chars().take_while(|c| *c == '.').count();
            let rest = &module[dots..];
            let mut dir: Vec<&str> = importer.split('/').collect();
            dir.pop();
            for _ in 1..dots {
                dir.pop()?;
            }
            let mut base = dir.join("/");
            if !rest.is_empty() {
                if !base.is_empty() {
                    base.push('/');
                }
                base.push_str(&rest.replace('.', "/"));
            }
            // Python looks for the package before the module of the same name.
            return [format!("{base}/__init__.py"), format!("{base}.py")]
                .iter()
                .find_map(|cand| ix.file(cand));
        }
        let cands = self.by_suffix.get(module)?;
        if cands.len() == 1 {
            return Some(cands[0]);
        }
        let service = importer.split('/').next().unwrap_or("");
        one(cands
            .iter()
            .copied()
            .filter(|c| c.path.split('/').next() == Some(service))
            .collect())
    }
}

/// The file a TypeScript or JavaScript specifier names. Only relative
/// specifiers can name a file of the repository; a bare one is a package.
///
/// `.`, `..` and a specifier with a trailing slash name a directory, so only
/// its index file fits: `..` from `web/src/app.ts` is `web/index.ts`, never
/// the file `web.ts` next to the directory.
fn script_module<'a>(ix: &Index<'a>, importer: &str, specifier: &str) -> Option<&'a FileRec> {
    let base = join_relative(importer, specifier)?;
    let script = |path: String| ix.file(&path).filter(|f| Lang::of(f) == Lang::Script);
    let directory = matches!(specifier.rsplit('/').next(), Some("" | "." | ".."));
    if !directory {
        if let Some(f) = script(base.clone()) {
            return Some(f);
        }
        if let Some(f) = SCRIPT_EXTENSIONS.iter().find_map(|ext| script(format!("{base}{ext}"))) {
            return Some(f);
        }
    }
    let dir = if base.is_empty() {
        String::new()
    } else {
        format!("{base}/")
    };
    if let Some(f) = SCRIPT_EXTENSIONS
        .iter()
        .find_map(|ext| script(format!("{dir}index{ext}")))
    {
        return Some(f);
    }
    if directory {
        return None;
    }
    // TypeScript sources name the compiled file: `./x.js` is `x.ts` on disk.
    for (written, sources) in [(".js", &[".ts", ".tsx"][..]), (".jsx", &[".tsx"][..])] {
        if let Some(stem) = base.strip_suffix(written) {
            if let Some(f) = sources.iter().find_map(|src| script(format!("{stem}{src}"))) {
                return Some(f);
            }
        }
    }
    None
}

/// `specifier` resolved against the directory of `importer`, as a path from the
/// indexed root. `None` for a bare specifier and for a path that leaves the root.
fn join_relative(importer: &str, specifier: &str) -> Option<String> {
    let relative = specifier == "." || specifier == ".." || specifier.starts_with("./") || specifier.starts_with("../");
    if !relative {
        return None;
    }
    let mut parts: Vec<&str> = importer.split('/').collect();
    parts.pop();
    for segment in specifier.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_specifiers_are_joined_to_the_importing_directory() {
        assert_eq!(
            join_relative("web/src/routes.ts", "./views").as_deref(),
            Some("web/src/views")
        );
        assert_eq!(
            join_relative("web/src/routes.ts", "../shared/util").as_deref(),
            Some("web/shared/util")
        );
        assert_eq!(join_relative("web/src/routes.ts", "../../lib/").as_deref(), Some("lib"));
        assert_eq!(join_relative("web/src/routes.ts", ".").as_deref(), Some("web/src"));
        assert_eq!(join_relative("web/src/routes.ts", "..").as_deref(), Some("web"));
        assert_eq!(join_relative("main.ts", "./a/./b").as_deref(), Some("a/b"));
        assert_eq!(join_relative("main.ts", ".").as_deref(), Some(""));
    }

    #[test]
    fn bare_specifiers_and_paths_above_the_root_name_no_file() {
        assert_eq!(join_relative("web/src/routes.ts", "express"), None);
        assert_eq!(join_relative("web/src/routes.ts", "@scope/pkg"), None);
        assert_eq!(join_relative("web/src/routes.ts", "/abs/path"), None);
        assert_eq!(join_relative("web/src/routes.ts", ".hidden"), None);
        assert_eq!(join_relative("main.ts", "../outside"), None);
        assert_eq!(join_relative("web/a.ts", "../../outside"), None);
    }

    #[test]
    fn a_name_bound_twice_to_different_things_is_ambiguous() {
        let mut map: HashMap<&str, Slot<&str>> = HashMap::new();
        bind(&mut map, "parse", "fast.py");
        bind(&mut map, "parse", "fast.py");
        assert_eq!(map["parse"], Slot::One("fast.py"));
        bind(&mut map, "parse", "slow.py");
        assert_eq!(map["parse"], Slot::Ambiguous);
        bind(&mut map, "parse", "fast.py");
        assert_eq!(map["parse"], Slot::Ambiguous);
    }
}
