//! What the indexer writes and the resolver reads: files, symbols and the
//! staged call, import and fact sites.

use crate::values::{col_b, col_i, col_s, count, int, rows, st, Prepared};
use anyhow::{anyhow, Result};
use lbug::{Connection, Value};
use lge_core::{service_id, CallSite, FactSite, FileInfo, FileRec, ImportSite, ParsedFile, ResolveInput, SymbolRec};
use std::collections::{HashMap, HashSet};

/// Files written per transaction by a bulk insert. A commit costs a few
/// milliseconds, so batching is what makes a first index of a large
/// repository fast; the batch is small enough to keep a transaction short.
pub(crate) const BULK_BATCH: usize = 200;

pub(crate) fn file_hashes(conn: &Connection) -> Result<HashMap<String, (String, String)>> {
    let mut out = HashMap::new();
    for r in rows(conn, "MATCH (f:File) RETURN f.path, f.id, f.content_hash;", vec![])? {
        out.insert(col_s(&r, 0), (col_s(&r, 1), col_s(&r, 2)));
    }
    Ok(out)
}

pub(crate) fn file_info(conn: &Connection, path: &str) -> Result<Option<FileInfo>> {
    let found = rows(
        conn,
        "MATCH (f:File) WHERE f.path = $p \
         RETURN f.id, f.path, f.service, f.lang, f.content_hash, f.indexed_commit, f.indexed_at;",
        vec![("p", st(path))],
    )?;
    Ok(found.first().map(|r| FileInfo {
        id: col_s(r, 0),
        path: col_s(r, 1),
        service: col_s(r, 2),
        lang: col_s(r, 3),
        content_hash: col_s(r, 4),
        indexed_commit: col_s(r, 5),
        indexed_at: col_i(r, 6),
    }))
}

/// Remove a file and everything extracted from it. Must run inside a transaction.
///
/// A Service that has no file left is removed too, unless it is `keep_service`
/// (the service the caller is about to write the file back into).
pub(crate) fn delete_file(conn: &Connection, path: &str, keep_service: Option<&str>) -> Result<()> {
    let p = || vec![("p", st(path))];
    let services: HashSet<String> = rows(conn, "MATCH (f:File) WHERE f.path = $p RETURN f.service;", p())?
        .iter()
        .map(|r| col_s(r, 0))
        .collect();
    for q in [
        // DETACH also removes every edge that touches the file's symbols.
        "MATCH (s:Symbol) WHERE s.file_path = $p DETACH DELETE s;",
        "MATCH (c:CallSite) WHERE c.file_path = $p DELETE c;",
        "MATCH (i:ImportSite) WHERE i.file_path = $p DELETE i;",
        "MATCH (x:FactSite) WHERE x.file_path = $p DELETE x;",
        "MATCH (f:File) WHERE f.path = $p DETACH DELETE f;",
    ] {
        rows(conn, q, p())?;
    }
    for service in services.iter().filter(|s| Some(s.as_str()) != keep_service) {
        let left = count(
            conn,
            "MATCH (f:File) WHERE f.service = $s RETURN count(f);",
            vec![("s", st(service))],
        )?;
        if left == 0 {
            rows(
                conn,
                "MATCH (s:Service) WHERE s.id = $id DETACH DELETE s;",
                vec![("id", st(&service_id(service)))],
            )?;
        }
    }
    Ok(())
}

/// True when the file sits under a directory named after its service
/// (`orders_api/routes.py`). That directory is then the service's root;
/// otherwise the whole repository is one service and its root is "".
fn in_service_dir(service: &str, file_path: &str) -> bool {
    !service.is_empty()
        && file_path
            .strip_prefix(service)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Writes parsed files inside the caller's transaction. The statements are
/// prepared once, so a bulk insert pays for planning once per batch.
pub(crate) struct FileWriter<'c, 'db> {
    conn: &'c Connection<'db>,
    commit: &'c str,
    now: i64,
    file_exists: Prepared<'c, 'db>,
    service_root: Prepared<'c, 'db>,
    file: Prepared<'c, 'db>,
    service_file: Prepared<'c, 'db>,
    symbol: Prepared<'c, 'db>,
    file_symbol: Prepared<'c, 'db>,
    call: Prepared<'c, 'db>,
    import: Prepared<'c, 'db>,
    fact: Prepared<'c, 'db>,
}

impl<'c, 'db> FileWriter<'c, 'db> {
    pub(crate) fn new(conn: &'c Connection<'db>, commit: &'c str, now: i64) -> Result<Self> {
        let prepare = |q: &str| Prepared::new(conn, q);
        Ok(FileWriter {
            conn,
            commit,
            now,
            file_exists: prepare("MATCH (f:File {id:$id}) RETURN f.path;")?,
            service_root: prepare("MATCH (s:Service {id:$id}) RETURN s.root;")?,
            file: prepare(
                "CREATE (:File {id:$id, path:$path, service:$service, lang:$lang, content_hash:$hash, \
                 indexed_commit:$commit, indexed_at:$at});",
            )?,
            service_file: prepare("MATCH (s:Service {id:$s}), (f:File {id:$f}) CREATE (s)-[:CONTAINS]->(f);")?,
            symbol: prepare(
                "CREATE (:Symbol {id:$id, name:$name, qualified_name:$qn, kind:$kind, signature:$sig, doc:$doc, \
                 body:$body, file_path:$fp, service:$svc, start_line:$sl, end_line:$el, is_test:$t});",
            )?,
            file_symbol: prepare("MATCH (f:File {id:$f}), (s:Symbol {id:$s}) CREATE (f)-[:CONTAINS]->(s);")?,
            call: prepare(
                "CREATE (:CallSite {id:$id, caller_id:$caller, file_path:$fp, name:$name, receiver:$recv, line:$line});",
            )?,
            import: prepare("CREATE (:ImportSite {id:$id, file_path:$fp, module:$m, name:$n, alias:$a});")?,
            fact: prepare(
                "CREATE (:FactSite {id:$id, symbol_id:$sym, file_path:$fp, kind:$kind, key:$key, detail:$detail, \
                 line:$line, end_line:$el, literal:$lit});",
            )?,
        })
    }

    /// Write one parsed file: the File node, its Service, its Symbols with
    /// their bodies, the CONTAINS edges and the staged sites. A file that is
    /// already indexed is replaced.
    pub(crate) fn write(&mut self, parsed: &ParsedFile) -> Result<()> {
        let file = parsed
            .file
            .as_ref()
            .ok_or_else(|| anyhow!("parsed file without FileRec"))?;
        for old in self.file_exists.rows(vec![("id", st(&file.id))])? {
            delete_file(self.conn, &col_s(&old, 0), Some(&file.service))?;
        }
        self.ensure_service(file)?;
        self.file.run(vec![
            ("id", st(&file.id)),
            ("path", st(&file.path)),
            ("service", st(&file.service)),
            ("lang", st(&file.lang)),
            ("hash", st(&file.content_hash)),
            ("commit", st(self.commit)),
            ("at", int(self.now)),
        ])?;
        self.service_file
            .run(vec![("s", st(&service_id(&file.service))), ("f", st(&file.id))])?;

        // An id is a hash of what the row is, so a second row with the same id
        // is the same row (a redefinition, an overload): the first one is kept.
        let mut seen: HashSet<&str> = HashSet::new();
        for s in parsed.symbols.iter().filter(|s| seen.insert(s.id.as_str())) {
            self.symbol.run(symbol_params(s))?;
            self.file_symbol.run(vec![("f", st(&file.id)), ("s", st(&s.id))])?;
        }
        seen.clear();
        for c in parsed.calls.iter().filter(|c| seen.insert(c.id.as_str())) {
            self.call.run(vec![
                ("id", st(&c.id)),
                ("caller", st(&c.caller_id)),
                ("fp", st(&c.file_path)),
                ("name", st(&c.name)),
                ("recv", st(&c.receiver)),
                ("line", int(c.line)),
            ])?;
        }
        seen.clear();
        for i in parsed.imports.iter().filter(|i| seen.insert(i.id.as_str())) {
            self.import.run(vec![
                ("id", st(&i.id)),
                ("fp", st(&i.file_path)),
                ("m", st(&i.module)),
                ("n", st(&i.name)),
                ("a", st(&i.alias)),
            ])?;
        }
        seen.clear();
        for f in parsed.facts.iter().filter(|f| seen.insert(f.id.as_str())) {
            self.fact.run(vec![
                ("id", st(&f.id)),
                ("sym", st(&f.symbol_id)),
                ("fp", st(&f.file_path)),
                ("kind", st(&f.kind)),
                ("key", st(&f.key)),
                ("detail", st(&f.detail)),
                ("line", int(f.line)),
                ("el", int(f.end_line)),
                ("lit", Value::Bool(f.literal)),
            ])?;
        }
        Ok(())
    }

    /// Create the Service of a file if it is missing and hang it under the
    /// Repository when one is recorded.
    fn ensure_service(&mut self, file: &FileRec) -> Result<()> {
        let id = service_id(&file.service);
        let under_own_dir = in_service_dir(&file.service, &file.path);
        match self.service_root.rows(vec![("id", st(&id))])?.first() {
            None => {
                let root = if under_own_dir { file.service.as_str() } else { "" };
                rows(
                    self.conn,
                    "CREATE (:Service {id:$id, name:$name, root:$root});",
                    vec![("id", st(&id)), ("name", st(&file.service)), ("root", st(root))],
                )?;
                rows(
                    self.conn,
                    "MATCH (r:Repository), (s:Service {id:$id}) MERGE (r)-[:CONTAINS]->(s);",
                    vec![("id", st(&id))],
                )?;
            }
            // A file outside the directory shows the service is not confined to it.
            Some(row) if !under_own_dir && !col_s(row, 0).is_empty() => {
                rows(
                    self.conn,
                    "MATCH (s:Service {id:$id}) SET s.root = '';",
                    vec![("id", st(&id))],
                )?;
            }
            Some(_) => {}
        }
        Ok(())
    }
}

fn symbol_params(s: &SymbolRec) -> Vec<(&'static str, Value)> {
    vec![
        ("id", st(&s.id)),
        ("name", st(&s.name)),
        ("qn", st(&s.qualified_name)),
        ("kind", st(&s.kind)),
        ("sig", st(&s.signature)),
        ("doc", st(&s.doc)),
        ("body", st(&s.body)),
        ("fp", st(&s.file_path)),
        ("svc", st(&s.service)),
        ("sl", int(s.start_line)),
        ("el", int(s.end_line)),
        ("t", Value::Bool(s.is_test)),
    ]
}

/// Everything the resolver needs, in a fixed order so that the same database
/// always gives the resolver the same input. Symbol bodies are not loaded and
/// placeholders are left out: they are the resolver's own output.
pub(crate) fn load_resolve_input(conn: &Connection) -> Result<ResolveInput> {
    let mut input = ResolveInput::default();
    for r in rows(
        conn,
        "MATCH (f:File) RETURN f.id, f.path, f.service, f.lang, f.content_hash;",
        vec![],
    )? {
        input.files.push(FileRec {
            id: col_s(&r, 0),
            path: col_s(&r, 1),
            service: col_s(&r, 2),
            lang: col_s(&r, 3),
            content_hash: col_s(&r, 4),
        });
    }
    for r in rows(
        conn,
        "MATCH (s:Symbol) WHERE s.kind <> 'unresolved' \
         RETURN s.id, s.name, s.qualified_name, s.kind, s.signature, s.doc, s.file_path, s.service, \
         s.start_line, s.end_line, s.is_test;",
        vec![],
    )? {
        input.symbols.push(SymbolRec {
            id: col_s(&r, 0),
            name: col_s(&r, 1),
            qualified_name: col_s(&r, 2),
            kind: col_s(&r, 3),
            signature: col_s(&r, 4),
            doc: col_s(&r, 5),
            body: String::new(),
            file_path: col_s(&r, 6),
            service: col_s(&r, 7),
            start_line: col_i(&r, 8),
            end_line: col_i(&r, 9),
            is_test: col_b(&r, 10),
        });
    }
    for r in rows(
        conn,
        "MATCH (c:CallSite) RETURN c.id, c.caller_id, c.file_path, c.name, c.receiver, c.line;",
        vec![],
    )? {
        input.calls.push(CallSite {
            id: col_s(&r, 0),
            caller_id: col_s(&r, 1),
            file_path: col_s(&r, 2),
            name: col_s(&r, 3),
            receiver: col_s(&r, 4),
            line: col_i(&r, 5),
        });
    }
    for r in rows(
        conn,
        "MATCH (i:ImportSite) RETURN i.id, i.file_path, i.module, i.name, i.alias;",
        vec![],
    )? {
        input.imports.push(ImportSite {
            id: col_s(&r, 0),
            file_path: col_s(&r, 1),
            module: col_s(&r, 2),
            name: col_s(&r, 3),
            alias: col_s(&r, 4),
        });
    }
    for r in rows(
        conn,
        "MATCH (x:FactSite) \
         RETURN x.id, x.symbol_id, x.file_path, x.kind, x.key, x.detail, x.line, x.end_line, x.literal;",
        vec![],
    )? {
        input.facts.push(FactSite {
            id: col_s(&r, 0),
            symbol_id: col_s(&r, 1),
            file_path: col_s(&r, 2),
            kind: col_s(&r, 3),
            key: col_s(&r, 4),
            detail: col_s(&r, 5),
            line: col_i(&r, 6),
            end_line: col_i(&r, 7),
            literal: col_b(&r, 8),
        });
    }
    input.files.sort_by(|a, b| a.path.cmp(&b.path));
    input
        .symbols
        .sort_by(|a, b| (&a.file_path, a.start_line, &a.id).cmp(&(&b.file_path, b.start_line, &b.id)));
    input
        .calls
        .sort_by(|a, b| (&a.file_path, a.line, &a.id).cmp(&(&b.file_path, b.line, &b.id)));
    input.imports.sort_by(|a, b| {
        (&a.file_path, &a.module, &a.name, &a.alias, &a.id).cmp(&(&b.file_path, &b.module, &b.name, &b.alias, &b.id))
    });
    input
        .facts
        .sort_by(|a, b| (&a.file_path, a.line, &a.id).cmp(&(&b.file_path, b.line, &b.id)));
    Ok(input)
}
