//! `tests/fixtures/services` must index exactly as it did before the crate was
//! split into modules: other crates assert the graph built from it.
//! `data/services_golden.txt` was written by the original single-file indexer.

use std::fmt::Write;
use std::path::Path;

#[test]
fn services_fixture_is_unchanged() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/services");
    let mut got = String::new();
    for f in lge_index::scan(&root, 1).unwrap() {
        assert_eq!(f.lang, "python", "{}", f.rel_path);
        let (bytes, hash) = lge_index::hash_file(&f.abs_path).unwrap();
        let p = lge_index::parse_python(&f.rel_path, &f.service, &bytes, &hash).unwrap();
        writeln!(got, "FILE {:?}", p.file).unwrap();
        for s in &p.symbols {
            writeln!(
                got,
                "SYM {} {} {} {} sig={:?} doc={:?} {} {} {}-{} test={}",
                s.id,
                s.name,
                s.qualified_name,
                s.kind,
                s.signature,
                s.doc,
                s.file_path,
                s.service,
                s.start_line,
                s.end_line,
                s.is_test
            )
            .unwrap();
        }
        for c in &p.calls {
            writeln!(got, "CALL {c:?}").unwrap();
        }
        for i in &p.imports {
            writeln!(got, "IMP {i:?}").unwrap();
        }
        // The fixture has no routes, SQL, queues, base classes or callbacks.
        assert_eq!(p.facts, vec![], "{}", f.rel_path);
        // Bodies are new: every definition has one, the module has none.
        for s in &p.symbols {
            assert_eq!(s.body.is_empty(), s.kind == "module", "{}", s.qualified_name);
        }
    }
    let want = include_str!("data/services_golden.txt");
    assert_eq!(got, want);
}
