//! LadybugDB adapter. No other crate talks to `lbug` directly.

use anyhow::{anyhow, Result};
use lbug::{Connection, Database, SystemConfig, Value};
use lge_core::{method, Hit, ParsedFile};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &[&str] = &[
    "CREATE NODE TABLE IF NOT EXISTS Meta(key STRING, value STRING, PRIMARY KEY(key));",
    "CREATE NODE TABLE IF NOT EXISTS File(id STRING, path STRING, service STRING, lang STRING, content_hash STRING, indexed_at INT64, PRIMARY KEY(id));",
    "CREATE NODE TABLE IF NOT EXISTS Symbol(id STRING, name STRING, qualified_name STRING, kind STRING, signature STRING, doc STRING, file_path STRING, service STRING, start_line INT64, end_line INT64, is_test BOOLEAN, PRIMARY KEY(id));",
    "CREATE NODE TABLE IF NOT EXISTS CallSite(id STRING, caller_id STRING, file_path STRING, name STRING, receiver STRING, line INT64, PRIMARY KEY(id));",
    "CREATE NODE TABLE IF NOT EXISTS ImportSite(id STRING, file_path STRING, module STRING, name STRING, alias STRING, PRIMARY KEY(id));",
    "CREATE REL TABLE IF NOT EXISTS CONTAINS(FROM File TO Symbol);",
    "CREATE REL TABLE IF NOT EXISTS IMPORTS(FROM File TO File, confidence DOUBLE, method STRING);",
    "CREATE REL TABLE IF NOT EXISTS CALLS(FROM Symbol TO Symbol, confidence DOUBLE, method STRING, evidence_file STRING, line INT64);",
];

const HIT_COLS: &str = "s.id, s.name, s.qualified_name, s.kind, s.file_path, s.service, s.start_line, s.end_line, s.is_test";

/// Method names too common to match by name alone (`x.get()` is usually a dict).
const COMMON_METHODS: &[&str] = &[
    "get", "set", "add", "append", "extend", "update", "items", "keys", "values", "format", "join", "split", "strip",
    "read", "write", "close", "open", "pop", "copy", "count", "index", "sort", "filter", "all", "first", "encode",
    "decode", "lower", "upper", "replace", "startswith", "endswith", "json", "dict", "run", "start", "stop",
];

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "when", "add", "fix", "update", "change", "make",
    "new", "bug", "feature", "implement", "support", "should", "need", "use", "not", "are", "can", "all", "any",
];

#[derive(Debug, Default, Clone)]
pub struct EdgeStats {
    pub calls_static: usize,
    pub calls_inferred: usize,
    pub calls_unresolved: usize,
    pub calls_external: usize,
    pub imports_resolved: usize,
    pub imports_external: usize,
}

pub struct Store {
    db: Database,
    /// True when the LadybugDB `fts` extension is loaded (BM25 search).
    pub fts: bool,
}

fn vs(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null(_) => String::new(),
        o => o.to_string(),
    }
}
fn vi(v: &Value) -> i64 {
    match v {
        Value::Int64(i) => *i,
        Value::Int32(i) => *i as i64,
        Value::UInt64(i) => *i as i64,
        _ => 0,
    }
}
fn vb(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}
fn vf(v: &Value) -> f64 {
    match v {
        Value::Double(f) => *f,
        Value::Float(f) => *f as f64,
        o => vi(o) as f64,
    }
}
fn st(s: &str) -> Value {
    Value::String(s.to_string())
}

fn hit(row: &[Value], score: f64) -> Hit {
    Hit {
        id: vs(&row[0]),
        name: vs(&row[1]),
        qualified_name: vs(&row[2]),
        kind: vs(&row[3]),
        file_path: vs(&row[4]),
        service: vs(&row[5]),
        start_line: vi(&row[6]),
        end_line: vi(&row[7]),
        is_test: vb(&row[8]),
        score,
    }
}

/// Words of a task description that are worth searching for.
pub fn tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in text.split(|c: char| !c.is_alphanumeric()) {
        let w = w.to_lowercase();
        if w.len() >= 3 && !STOPWORDS.contains(&w.as_str()) && !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

struct Sym {
    id: String,
    name: String,
    qn: String,
    kind: String,
    file: String,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let db = Database::new(path, SystemConfig::default())?;
        let mut store = Store { db, fts: false };
        let fts = {
            let conn = store.conn()?;
            for ddl in SCHEMA {
                conn.query(ddl)?;
            }
            conn.query(&format!("MERGE (m:Meta {{key:'schema_version'}}) SET m.value='{SCHEMA_VERSION}';"))?;
            // The extension must have been installed once with `lge setup`.
            conn.query("LOAD fts;").is_ok()
        };
        store.fts = fts;
        Ok(store)
    }

    pub fn conn(&self) -> Result<Connection<'_>> {
        Ok(Connection::new(&self.db)?)
    }

    /// Download and install the BM25 extension (needs network once).
    pub fn install_fts(&mut self) -> Result<()> {
        let conn = self.conn()?;
        conn.query("INSTALL fts;")?;
        conn.query("LOAD fts;")?;
        drop(conn);
        self.fts = true;
        self.refresh_fts()
    }

    fn refresh_fts(&self) -> Result<()> {
        if !self.fts {
            return Ok(());
        }
        let conn = self.conn()?;
        let _ = conn.query("CALL DROP_FTS_INDEX('Symbol', 'sym_fts');");
        conn.query("CALL CREATE_FTS_INDEX('Symbol', 'sym_fts', ['name', 'qualified_name', 'signature', 'doc']);")?;
        Ok(())
    }

    /// path -> (file id, content hash) for every indexed file.
    pub fn file_hashes(&self) -> Result<HashMap<String, (String, String)>> {
        let conn = self.conn()?;
        let mut out = HashMap::new();
        for row in conn.query("MATCH (f:File) RETURN f.path, f.id, f.content_hash;")? {
            out.insert(vs(&row[0]), (vs(&row[1]), vs(&row[2])));
        }
        Ok(out)
    }

    /// Remove a file and everything extracted from it.
    pub fn delete_file(&self, path: &str) -> Result<()> {
        let conn = self.conn()?;
        for q in [
            "MATCH (s:Symbol) WHERE s.file_path = $p DETACH DELETE s;",
            "MATCH (c:CallSite) WHERE c.file_path = $p DELETE c;",
            "MATCH (i:ImportSite) WHERE i.file_path = $p DELETE i;",
            "MATCH (f:File) WHERE f.path = $p DETACH DELETE f;",
        ] {
            let mut stmt = conn.prepare(q)?;
            conn.execute(&mut stmt, vec![("p", st(path))])?;
        }
        Ok(())
    }

    pub fn insert_parsed(&self, parsed: &ParsedFile, now: i64) -> Result<()> {
        let file = parsed.file.as_ref().ok_or_else(|| anyhow!("parsed file without FileRec"))?;
        let conn = self.conn()?;
        conn.query("BEGIN TRANSACTION;")?;
        let res = (|| -> Result<()> {
            let mut f = conn.prepare("CREATE (:File {id:$id, path:$path, service:$service, lang:$lang, content_hash:$hash, indexed_at:$at});")?;
            conn.execute(
                &mut f,
                vec![
                    ("id", st(&file.id)),
                    ("path", st(&file.path)),
                    ("service", st(&file.service)),
                    ("lang", st(&file.lang)),
                    ("hash", st(&file.content_hash)),
                    ("at", Value::Int64(now)),
                ],
            )?;
            let mut s = conn.prepare("CREATE (:Symbol {id:$id, name:$name, qualified_name:$qn, kind:$kind, signature:$sig, doc:$doc, file_path:$fp, service:$svc, start_line:$sl, end_line:$el, is_test:$t});")?;
            let mut link = conn.prepare("MATCH (f:File {id:$f}), (s:Symbol {id:$s}) CREATE (f)-[:CONTAINS]->(s);")?;
            for sym in &parsed.symbols {
                conn.execute(
                    &mut s,
                    vec![
                        ("id", st(&sym.id)),
                        ("name", st(&sym.name)),
                        ("qn", st(&sym.qualified_name)),
                        ("kind", st(&sym.kind)),
                        ("sig", st(&sym.signature)),
                        ("doc", st(&sym.doc)),
                        ("fp", st(&sym.file_path)),
                        ("svc", st(&sym.service)),
                        ("sl", Value::Int64(sym.start_line)),
                        ("el", Value::Int64(sym.end_line)),
                        ("t", Value::Bool(sym.is_test)),
                    ],
                )?;
                conn.execute(&mut link, vec![("f", st(&file.id)), ("s", st(&sym.id))])?;
            }
            let mut c = conn.prepare("CREATE (:CallSite {id:$id, caller_id:$caller, file_path:$fp, name:$name, receiver:$recv, line:$line});")?;
            for call in &parsed.calls {
                conn.execute(
                    &mut c,
                    vec![
                        ("id", st(&call.id)),
                        ("caller", st(&call.caller_id)),
                        ("fp", st(&call.file_path)),
                        ("name", st(&call.name)),
                        ("recv", st(&call.receiver)),
                        ("line", Value::Int64(call.line)),
                    ],
                )?;
            }
            let mut i = conn.prepare("CREATE (:ImportSite {id:$id, file_path:$fp, module:$m, name:$n, alias:$a});")?;
            for imp in &parsed.imports {
                conn.execute(
                    &mut i,
                    vec![("id", st(&imp.id)), ("fp", st(&imp.file_path)), ("m", st(&imp.module)), ("n", st(&imp.name)), ("a", st(&imp.alias))],
                )?;
            }
            Ok(())
        })();
        match res {
            Ok(()) => {
                conn.query("COMMIT;")?;
                Ok(())
            }
            Err(e) => {
                let _ = conn.query("ROLLBACK;");
                Err(e)
            }
        }
    }

    /// Rebuild IMPORTS and CALLS from the stored import and call sites.
    ///
    /// Every edge records how it was found. Calls that match several symbols
    /// are stored as unresolved; they are never guessed.
    pub fn rebuild_edges(&self) -> Result<EdgeStats> {
        let conn = self.conn()?;
        conn.query("MATCH ()-[c:CALLS]->() DELETE c;")?;
        conn.query("MATCH ()-[i:IMPORTS]->() DELETE i;")?;
        conn.query("MATCH (s:Symbol) WHERE s.kind = 'unresolved' DELETE s;")?;

        let mut syms: Vec<Sym> = Vec::new();
        for r in conn.query("MATCH (s:Symbol) RETURN s.id, s.name, s.qualified_name, s.kind, s.file_path;")? {
            syms.push(Sym { id: vs(&r[0]), name: vs(&r[1]), qn: vs(&r[2]), kind: vs(&r[3]), file: vs(&r[4]) });
        }
        let mut files: BTreeMap<String, String> = BTreeMap::new(); // path -> id
        for r in conn.query("MATCH (f:File) RETURN f.path, f.id;")? {
            files.insert(vs(&r[0]), vs(&r[1]));
        }

        let by_id: HashMap<&str, &Sym> = syms.iter().map(|s| (s.id.as_str(), s)).collect();
        let by_qn: HashMap<&str, &Sym> = syms.iter().map(|s| (s.qn.as_str(), s)).collect();
        let mut by_file_name: HashMap<(&str, &str), Vec<&Sym>> = HashMap::new();
        let mut by_name: HashMap<&str, Vec<&Sym>> = HashMap::new();
        for s in syms.iter().filter(|s| s.kind != "module") {
            by_file_name.entry((s.file.as_str(), s.name.as_str())).or_default().push(s);
            by_name.entry(s.name.as_str()).or_default().push(s);
        }

        // Dotted module suffix -> files. "svc/app/tasks.py" answers to
        // "svc.app.tasks", "app.tasks" and "tasks".
        let mut modules: HashMap<String, Vec<&str>> = HashMap::new();
        for path in files.keys() {
            let trimmed = path.trim_end_matches(".py").trim_end_matches("/__init__");
            let parts: Vec<&str> = trimmed.split('/').collect();
            for i in 0..parts.len() {
                modules.entry(parts[i..].join(".")).or_default().push(path.as_str());
            }
        }
        let resolve_module = |importer: &str, module: &str| -> Option<String> {
            if module.is_empty() {
                return None;
            }
            if module.starts_with('.') {
                let dots = module.chars().take_while(|c| *c == '.').count();
                let rest = &module[dots..];
                let mut dir: Vec<&str> = importer.split('/').collect();
                dir.pop();
                for _ in 1..dots {
                    dir.pop();
                }
                let mut base = dir.join("/");
                if !rest.is_empty() {
                    if !base.is_empty() {
                        base.push('/');
                    }
                    base.push_str(&rest.replace('.', "/"));
                }
                for cand in [format!("{base}.py"), format!("{base}/__init__.py")] {
                    if files.contains_key(&cand) {
                        return Some(cand);
                    }
                }
                return None;
            }
            let cands = modules.get(module)?;
            if cands.len() == 1 {
                return Some(cands[0].to_string());
            }
            let svc = importer.split('/').next().unwrap_or("");
            let same: Vec<&&str> = cands.iter().filter(|c| c.split('/').next() == Some(svc)).collect();
            if same.len() == 1 {
                Some(same[0].to_string())
            } else {
                None
            }
        };

        let mut stats = EdgeStats::default();
        // (file, local name) -> (target file, original name)
        let mut name_imports: HashMap<(String, String), (String, String)> = HashMap::new();
        // (file, local name) -> target module file
        let mut mod_imports: HashMap<(String, String), String> = HashMap::new();
        let mut import_edges: HashSet<(String, String)> = HashSet::new();
        for r in conn.query("MATCH (i:ImportSite) RETURN i.file_path, i.module, i.name, i.alias;")? {
            let (file, module, name, alias) = (vs(&r[0]), vs(&r[1]), vs(&r[2]), vs(&r[3]));
            let mut target = None;
            if !name.is_empty() {
                let sub = if module.ends_with('.') { format!("{module}{name}") } else { format!("{module}.{name}") };
                if let Some(t) = resolve_module(&file, &sub) {
                    mod_imports.insert((file.clone(), alias.clone()), t.clone());
                    target = Some(t);
                } else if let Some(t) = resolve_module(&file, &module) {
                    name_imports.insert((file.clone(), alias.clone()), (t.clone(), name.clone()));
                    target = Some(t);
                }
            } else if let Some(t) = resolve_module(&file, &module) {
                mod_imports.insert((file.clone(), alias.clone()), t.clone());
                target = Some(t);
            }
            match target {
                Some(t) if t != file => {
                    import_edges.insert((file, t));
                    stats.imports_resolved += 1;
                }
                Some(_) => {}
                None => stats.imports_external += 1,
            }
        }

        let callable = |s: &&&Sym| s.kind == "function" || s.kind == "class";
        let one = |v: Vec<&&Sym>| -> Option<String> { if v.len() == 1 { Some(v[0].id.clone()) } else { None } };

        // (caller, callee) -> (confidence, method, evidence file, line)
        let mut edges: HashMap<(String, String), (f64, &'static str, String, i64)> = HashMap::new();
        let mut placeholders: HashSet<String> = HashSet::new();
        for r in conn.query("MATCH (c:CallSite) RETURN c.caller_id, c.file_path, c.name, c.receiver, c.line;")? {
            let (caller, file, name, recv, line) = (vs(&r[0]), vs(&r[1]), vs(&r[2]), vs(&r[3]), vi(&r[4]));
            let Some(caller_sym) = by_id.get(caller.as_str()) else { continue };
            let in_file = |f: &str, n: &str| -> Option<String> {
                by_file_name.get(&(f, n)).and_then(|v| one(v.iter().filter(callable).collect()))
            };
            let method_of = |class_file: &str, class_name: &str| -> Option<String> {
                let class = by_file_name.get(&(class_file, class_name))?.iter().find(|s| s.kind == "class")?;
                by_qn.get(format!("{}.{}", class.qn, name).as_str()).map(|s| s.id.clone())
            };

            let mut found: Option<(String, f64, &'static str)> = None;
            if recv == "self" {
                if let Some((class_qn, _)) = caller_sym.qn.rsplit_once('.') {
                    if let Some(s) = by_qn.get(format!("{class_qn}.{name}").as_str()) {
                        found = Some((s.id.clone(), 0.95, method::STATIC));
                    }
                }
            } else if recv.is_empty() {
                if let Some(id) = in_file(&file, &name) {
                    found = Some((id, 0.95, method::STATIC));
                } else if let Some((t, orig)) = name_imports.get(&(file.clone(), name.clone())) {
                    if let Some(id) = in_file(t, orig) {
                        found = Some((id, 0.9, method::STATIC));
                    }
                }
            } else {
                let class_name = recv.trim_end_matches("()");
                if let Some(t) = mod_imports.get(&(file.clone(), recv.clone())) {
                    if let Some(id) = in_file(t, &name) {
                        found = Some((id, 0.9, method::STATIC));
                    }
                } else if let Some((t, orig)) = name_imports.get(&(file.clone(), class_name.to_string())) {
                    if let Some(id) = method_of(t, orig) {
                        found = Some((id, 0.9, method::STATIC));
                    }
                } else if let Some(id) = method_of(&file, class_name) {
                    found = Some((id, 0.9, method::STATIC));
                }
            }

            if found.is_none() {
                // Fall back to a repository-wide name match.
                let cands: Vec<&&Sym> = by_name
                    .get(name.as_str())
                    .map(|v| {
                        v.iter()
                            .filter(|s| if recv.is_empty() { s.kind == "function" || s.kind == "class" } else { s.kind == "method" || s.kind == "function" })
                            .collect()
                    })
                    .unwrap_or_default();
                let too_common = !recv.is_empty() && (name.len() < 4 || COMMON_METHODS.contains(&name.as_str()));
                if cands.is_empty() || too_common {
                    stats.calls_external += 1;
                    continue;
                }
                if cands.len() == 1 {
                    found = Some((cands[0].id.clone(), if recv.is_empty() { 0.5 } else { 0.4 }, method::INFERRED));
                } else {
                    let ph = format!("unresolved:{name}");
                    placeholders.insert(name.clone());
                    found = Some((ph, 0.0, method::UNRESOLVED));
                }
            }

            let (callee, conf, how) = found.unwrap();
            if callee == caller {
                continue;
            }
            let e = edges.entry((caller, callee)).or_insert((conf, how, file.clone(), line));
            if conf > e.0 {
                *e = (conf, how, file, line);
            }
        }

        conn.query("BEGIN TRANSACTION;")?;
        let mut ph = conn.prepare("CREATE (:Symbol {id:$id, name:$name, qualified_name:$name, kind:'unresolved', signature:'', doc:'', file_path:'', service:'', start_line:0, end_line:0, is_test:false});")?;
        for name in &placeholders {
            conn.execute(&mut ph, vec![("id", st(&format!("unresolved:{name}"))), ("name", st(name))])?;
        }
        let mut imp = conn.prepare("MATCH (a:File {id:$a}), (b:File {id:$b}) CREATE (a)-[:IMPORTS {confidence:0.9, method:'static'}]->(b);")?;
        for (a, b) in &import_edges {
            conn.execute(&mut imp, vec![("a", st(&files[a])), ("b", st(&files[b]))])?;
        }
        let mut call = conn.prepare("MATCH (a:Symbol {id:$a}), (b:Symbol {id:$b}) CREATE (a)-[:CALLS {confidence:$c, method:$m, evidence_file:$f, line:$l}]->(b);")?;
        for ((a, b), (conf, how, file, line)) in &edges {
            conn.execute(
                &mut call,
                vec![("a", st(a)), ("b", st(b)), ("c", Value::Double(*conf)), ("m", st(how)), ("f", st(file)), ("l", Value::Int64(*line))],
            )?;
            match *how {
                method::STATIC => stats.calls_static += 1,
                method::INFERRED => stats.calls_inferred += 1,
                _ => stats.calls_unresolved += 1,
            }
        }
        conn.query("COMMIT;")?;
        drop(conn);
        self.refresh_fts()?;
        Ok(stats)
    }

    /// Seed search. BM25 through the fts extension when it is loaded,
    /// otherwise a substring match scored in Rust.
    pub fn search(&self, text: &str, limit: usize) -> Result<Vec<Hit>> {
        if self.fts {
            if let Ok(hits) = self.search_fts(text, limit) {
                if !hits.is_empty() {
                    return Ok(hits);
                }
            }
        }
        self.search_substring(text, limit)
    }

    fn search_fts(&self, text: &str, limit: usize) -> Result<Vec<Hit>> {
        let conn = self.conn()?;
        let q = format!(
            "CALL QUERY_FTS_INDEX('Symbol', 'sym_fts', $q) WITH node AS s, score WHERE s.kind <> 'module' AND s.kind <> 'unresolved' RETURN {HIT_COLS}, score ORDER BY score DESC LIMIT {limit};"
        );
        let mut stmt = conn.prepare(&q)?;
        let rows = conn.execute(&mut stmt, vec![("q", st(&tokens(text).join(" ")))])?;
        Ok(rows.map(|r| hit(&r, vf(&r[9]))).collect())
    }

    fn search_substring(&self, text: &str, limit: usize) -> Result<Vec<Hit>> {
        let conn = self.conn()?;
        let q = format!(
            "MATCH (s:Symbol) WHERE s.kind <> 'module' AND s.kind <> 'unresolved' AND (lower(s.name) CONTAINS $t OR lower(s.qualified_name) CONTAINS $t OR lower(s.doc) CONTAINS $t) RETURN {HIT_COLS}, s.doc;"
        );
        let mut stmt = conn.prepare(&q)?;
        let mut scored: HashMap<String, Hit> = HashMap::new();
        for t in tokens(text) {
            for r in conn.execute(&mut stmt, vec![("t", st(&t))])? {
                let name = vs(&r[1]).to_lowercase();
                let words: Vec<&str> = name.split('_').collect();
                let mut score = 0.0;
                if name == t {
                    score += 5.0;
                } else if words.contains(&t.as_str()) {
                    score += 4.0;
                } else if name.contains(&t) {
                    score += 3.0;
                } else if vs(&r[2]).to_lowercase().contains(&t) {
                    score += 1.5;
                }
                if vs(&r[9]).to_lowercase().contains(&t) {
                    score += 1.0;
                }
                scored.entry(vs(&r[0])).or_insert_with(|| hit(&r, 0.0)).score += score;
            }
        }
        let mut hits: Vec<Hit> = scored.into_values().collect();
        // Prefer production code over tests when scores tie.
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.is_test.cmp(&b.is_test)).then(a.qualified_name.cmp(&b.qualified_name)));
        hits.truncate(limit);
        Ok(hits)
    }

    /// Symbols by exact id, qualified name or name.
    pub fn find(&self, needle: &str) -> Result<Vec<Hit>> {
        let conn = self.conn()?;
        let q = format!("MATCH (s:Symbol) WHERE s.kind <> 'unresolved' AND (s.id = $n OR s.qualified_name = $n OR s.name = $n) RETURN {HIT_COLS};");
        let mut stmt = conn.prepare(&q)?;
        Ok(conn.execute(&mut stmt, vec![("n", st(needle))])?.map(|r| hit(&r, 0.0)).collect())
    }

    /// Reverse dependency traversal: everything that reaches `id` through
    /// CALLS within `depth` hops. Score is the lowest hop count.
    pub fn callers(&self, id: &str, depth: u8) -> Result<Vec<Hit>> {
        let conn = self.conn()?;
        let mut out: HashMap<String, Hit> = HashMap::new();
        for d in 1..=depth.clamp(1, 6) {
            let q = format!("MATCH (s:Symbol)-[:CALLS*{d}..{d}]->(t:Symbol) WHERE t.id = $id AND s.kind <> 'unresolved' RETURN DISTINCT {HIT_COLS};");
            let mut stmt = conn.prepare(&q)?;
            for r in conn.execute(&mut stmt, vec![("id", st(id))])? {
                out.entry(vs(&r[0])).or_insert_with(|| hit(&r, d as f64));
            }
        }
        let mut hits: Vec<Hit> = out.into_values().filter(|h| h.id != id).collect();
        hits.sort_by(|a, b| a.score.total_cmp(&b.score).then(a.qualified_name.cmp(&b.qualified_name)));
        Ok(hits)
    }

    /// Number of unresolved call edges leaving a symbol.
    pub fn unresolved_out(&self, id: &str) -> Result<i64> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("MATCH (s:Symbol)-[c:CALLS]->(:Symbol) WHERE s.id = $id AND c.method = 'unresolved' RETURN count(c);")?;
        let n = conn.execute(&mut stmt, vec![("id", st(id))])?.next().map(|r| vi(&r[0])).unwrap_or(0);
        Ok(n)
    }

    pub fn stats(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.conn()?;
        let mut out = Vec::new();
        for (label, q) in [
            ("files", "MATCH (f:File) RETURN count(f);"),
            ("symbols", "MATCH (s:Symbol) WHERE s.kind <> 'unresolved' RETURN count(s);"),
            ("call_sites", "MATCH (c:CallSite) RETURN count(c);"),
            ("calls_static", "MATCH ()-[c:CALLS]->() WHERE c.method = 'static' RETURN count(c);"),
            ("calls_inferred", "MATCH ()-[c:CALLS]->() WHERE c.method = 'inferred' RETURN count(c);"),
            ("calls_unresolved", "MATCH ()-[c:CALLS]->() WHERE c.method = 'unresolved' RETURN count(c);"),
            ("imports", "MATCH ()-[i:IMPORTS]->() RETURN count(i);"),
        ] {
            let n = conn.query(q)?.next().map(|r| vi(&r[0])).unwrap_or(0);
            out.push((label.to_string(), n));
        }
        Ok(out)
    }

    /// Run a raw Cypher query and return the printed result table.
    pub fn raw(&self, cypher: &str) -> Result<String> {
        let conn = self.conn()?;
        Ok(conn.query(cypher)?.to_string())
    }
}
