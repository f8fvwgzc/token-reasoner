//! Repository scanner and Tree-sitter extraction (Python first).

use anyhow::{Context, Result};
use lge_core::{content_hash, stable_id, CallSite, FileRec, ImportSite, ParsedFile, SymbolRec};
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser};

pub struct ScannedFile {
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub service: String,
}

/// Walk `root`, honouring .gitignore, and return Python source files.
///
/// `service_depth` 0 treats the whole root as one service; 1 uses the first
/// path component (a directory of sibling service repos).
pub fn scan(root: &Path, service_depth: usize) -> Result<Vec<ScannedFile>> {
    let root = root.canonicalize().context("root path")?;
    let root_name = root.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(&root).build() {
        let entry = entry?;
        let p = entry.path();
        if !p.is_file() || p.extension().and_then(|e| e.to_str()) != Some("py") {
            continue;
        }
        let rel = p.strip_prefix(&root)?.to_string_lossy().replace('\\', "/");
        let service = match (service_depth, rel.split_once('/')) {
            (1, Some((first, _))) => first.to_string(),
            _ => root_name.clone(),
        };
        out.push(ScannedFile { rel_path: rel, abs_path: p.to_path_buf(), service });
    }
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(out)
}

pub fn hash_file(path: &Path) -> Result<(Vec<u8>, String)> {
    let bytes = std::fs::read(path)?;
    let h = content_hash(&bytes);
    Ok((bytes, h))
}

struct Ctx<'a> {
    src: &'a [u8],
    path: &'a str,
    service: &'a str,
    file_is_test: bool,
    out: ParsedFile,
}

fn text<'a>(n: Node, src: &'a [u8]) -> &'a str {
    n.utf8_text(src).unwrap_or("")
}

fn module_name(path: &str) -> String {
    let p = path.trim_end_matches(".py").trim_end_matches("/__init__");
    p.replace('/', ".")
}

/// Parse one Python file into symbols, call sites and import sites.
pub fn parse_python(path: &str, service: &str, src: &[u8], hash: &str) -> Result<ParsedFile> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_python::LANGUAGE.into())?;
    let tree = parser.parse(src, None).context("tree-sitter parse failed")?;
    let base = path.rsplit('/').next().unwrap_or(path);
    let mut ctx = Ctx {
        src,
        path,
        service,
        file_is_test: base.starts_with("test_") || base.ends_with("_test.py") || path.contains("/tests/") || path.starts_with("tests/"),
        out: ParsedFile::default(),
    };
    ctx.out.file = Some(FileRec {
        id: stable_id(&["file", path]),
        path: path.to_string(),
        service: service.to_string(),
        lang: "python".into(),
        content_hash: hash.to_string(),
    });
    let module = module_name(path);
    let root = tree.root_node();
    let module_id = stable_id(&["symbol", path, &module, "module"]);
    ctx.out.symbols.push(SymbolRec {
        id: module_id.clone(),
        name: module.rsplit('.').next().unwrap_or(&module).to_string(),
        qualified_name: module.clone(),
        kind: "module".into(),
        signature: String::new(),
        doc: docstring(root, src),
        file_path: path.to_string(),
        service: service.to_string(),
        start_line: 1,
        end_line: root.end_position().row as i64 + 1,
        is_test: ctx.file_is_test,
    });
    visit(root, &mut ctx, &module, &module_id, false);
    Ok(ctx.out)
}

fn docstring(body: Node, src: &[u8]) -> String {
    let mut c = body.walk();
    let first = body.named_children(&mut c).find(|n| n.kind() != "comment");
    if let Some(stmt) = first {
        if stmt.kind() == "expression_statement" {
            if let Some(s) = stmt.named_child(0) {
                if s.kind() == "string" {
                    let t = text(s, src).trim_matches(|ch| ch == '"' || ch == '\'');
                    let line = t.trim().lines().next().unwrap_or("").trim();
                    return line.chars().take(200).collect();
                }
            }
        }
    }
    String::new()
}

fn visit(node: Node, ctx: &mut Ctx, scope: &str, owner_id: &str, in_class: bool) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "decorated_definition" => {
                // Decorator expressions are calls made by the enclosing scope.
                let mut c2 = child.walk();
                for d in child.named_children(&mut c2) {
                    if d.kind() == "decorator" {
                        visit(d, ctx, scope, owner_id, in_class);
                    }
                }
                if let Some(def) = child.child_by_field_name("definition") {
                    define(def, child, ctx, scope, in_class);
                }
            }
            "function_definition" | "class_definition" => define(child, child, ctx, scope, in_class),
            "call" => {
                record_call(child, ctx, owner_id);
                visit(child, ctx, scope, owner_id, in_class);
            }
            "import_statement" => record_import(child, ctx),
            "import_from_statement" => record_import_from(child, ctx),
            _ => visit(child, ctx, scope, owner_id, in_class),
        }
    }
}

fn define(def: Node, outer: Node, ctx: &mut Ctx, scope: &str, in_class: bool) {
    let Some(name_node) = def.child_by_field_name("name") else { return };
    let name = text(name_node, ctx.src).to_string();
    let is_class = def.kind() == "class_definition";
    let kind = if is_class { "class" } else if in_class { "method" } else { "function" };
    let qn = format!("{scope}.{name}");
    let id = stable_id(&["symbol", ctx.path, &qn, kind]);
    let body = def.child_by_field_name("body");
    let sig_end = body.map(|b| b.start_byte()).unwrap_or(def.end_byte());
    let signature: String = String::from_utf8_lossy(&ctx.src[def.start_byte()..sig_end])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(':')
        .chars()
        .take(240)
        .collect();
    ctx.out.symbols.push(SymbolRec {
        id: id.clone(),
        name: name.clone(),
        qualified_name: qn.clone(),
        kind: kind.into(),
        signature,
        doc: body.map(|b| docstring(b, ctx.src)).unwrap_or_default(),
        file_path: ctx.path.to_string(),
        service: ctx.service.to_string(),
        start_line: outer.start_position().row as i64 + 1,
        end_line: outer.end_position().row as i64 + 1,
        is_test: ctx.file_is_test || name.starts_with("test_") || (is_class && name.starts_with("Test")),
    });
    // Base classes and default arguments are evaluated in the outer scope,
    // but for the call graph we attribute everything inside to this symbol.
    visit(def, ctx, &qn, &id, is_class);
}

fn record_call(call: Node, ctx: &mut Ctx, owner_id: &str) {
    let Some(f) = call.child_by_field_name("function") else { return };
    let (name, receiver) = match f.kind() {
        "identifier" => (text(f, ctx.src).to_string(), String::new()),
        "attribute" => {
            let attr = f.child_by_field_name("attribute").map(|a| text(a, ctx.src)).unwrap_or("");
            let obj = f.child_by_field_name("object").map(|o| text(o, ctx.src)).unwrap_or("");
            let recv = if obj == "self" || obj == "cls" { "self".to_string() } else { obj.chars().take(80).collect() };
            (attr.to_string(), recv)
        }
        _ => return,
    };
    if name.is_empty() {
        return;
    }
    let line = call.start_position().row as i64 + 1;
    let col = call.start_position().column.to_string();
    ctx.out.calls.push(CallSite {
        id: stable_id(&["call", ctx.path, owner_id, &name, &line.to_string(), &col]),
        caller_id: owner_id.to_string(),
        file_path: ctx.path.to_string(),
        name,
        receiver,
        line,
    });
}

fn push_import(ctx: &mut Ctx, module: &str, name: &str, alias: &str) {
    ctx.out.imports.push(ImportSite {
        id: stable_id(&["import", ctx.path, module, name, alias]),
        file_path: ctx.path.to_string(),
        module: module.to_string(),
        name: name.to_string(),
        alias: alias.to_string(),
    });
}

fn record_import(node: Node, ctx: &mut Ctx) {
    let mut c = node.walk();
    for n in node.children_by_field_name("name", &mut c) {
        match n.kind() {
            "dotted_name" => {
                let m = text(n, ctx.src).to_string();
                // `import a.b` binds `a`, but calls are written `a.b.f()`.
                push_import(ctx, &m, "", &m);
            }
            "aliased_import" => {
                let m = n.child_by_field_name("name").map(|x| text(x, ctx.src)).unwrap_or("").to_string();
                let a = n.child_by_field_name("alias").map(|x| text(x, ctx.src)).unwrap_or("").to_string();
                push_import(ctx, &m, "", &a);
            }
            _ => {}
        }
    }
}

fn record_import_from(node: Node, ctx: &mut Ctx) {
    let module = node.child_by_field_name("module_name").map(|m| text(m, ctx.src)).unwrap_or("").to_string();
    let mut c = node.walk();
    for n in node.children_by_field_name("name", &mut c) {
        match n.kind() {
            "dotted_name" => {
                let name = text(n, ctx.src).to_string();
                push_import(ctx, &module, &name, &name);
            }
            "aliased_import" => {
                let name = n.child_by_field_name("name").map(|x| text(x, ctx.src)).unwrap_or("").to_string();
                let a = n.child_by_field_name("alias").map(|x| text(x, ctx.src)).unwrap_or("").to_string();
                push_import(ctx, &module, &name, &a);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_symbols_calls_imports() {
        let src = br#"
"""Task service."""
from .repo import TaskRepository as Repo
import auth.tokens

class TaskService:
    """Business logic."""
    def create_task(self, title):
        auth.tokens.verify(title)
        self.validate(title)
        return Repo().save(title)

    def validate(self, title):
        return len(title) > 0
"#;
        let p = parse_python("work/service.py", "work", src, "h").unwrap();
        let names: Vec<_> = p.symbols.iter().map(|s| (s.qualified_name.as_str(), s.kind.as_str())).collect();
        assert!(names.contains(&("work.service", "module")));
        assert!(names.contains(&("work.service.TaskService", "class")));
        assert!(names.contains(&("work.service.TaskService.create_task", "method")));
        let calls: Vec<_> = p.calls.iter().map(|c| (c.name.as_str(), c.receiver.as_str())).collect();
        assert!(calls.contains(&("verify", "auth.tokens")));
        assert!(calls.contains(&("validate", "self")));
        assert!(calls.contains(&("Repo", "")));
        assert!(calls.contains(&("save", "Repo()")));
        let imps: Vec<_> = p.imports.iter().map(|i| (i.module.as_str(), i.name.as_str(), i.alias.as_str())).collect();
        assert!(imps.contains(&(".repo", "TaskRepository", "Repo")));
        assert!(imps.contains(&("auth.tokens", "", "auth.tokens")));
        assert_eq!(p.symbols[0].doc, "Task service.");
    }
}
