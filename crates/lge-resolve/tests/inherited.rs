//! `self.method()` reaching a method that a base class defines.
use lge_core::{
    fact, file_id, kind, lang, method, symbol_id, CallSite, EdgeType, FactSite, FileRec, ResolveInput, SymbolRec,
};
use lge_resolve::resolve;

fn file(path: &str) -> FileRec {
    FileRec {
        id: file_id(path),
        path: path.into(),
        service: "svc".into(),
        lang: lang::PYTHON.into(),
        content_hash: "h".into(),
    }
}

fn sym(path: &str, qn: &str, k: &str) -> SymbolRec {
    SymbolRec {
        id: symbol_id(path, qn, k),
        name: qn.rsplit('.').next().unwrap().into(),
        qualified_name: qn.into(),
        kind: k.into(),
        file_path: path.into(),
        service: "svc".into(),
        start_line: 1,
        end_line: 2,
        ..SymbolRec::default()
    }
}

fn inherits(path: &str, class_qn: &str, base: &str) -> FactSite {
    FactSite {
        id: format!("{class_qn}<{base}"),
        symbol_id: symbol_id(path, class_qn, kind::CLASS),
        file_path: path.into(),
        kind: fact::INHERITS.into(),
        key: base.into(),
        line: 1,
        end_line: 1,
        literal: true,
        ..FactSite::default()
    }
}

fn self_call(path: &str, caller_qn: &str, name: &str) -> CallSite {
    CallSite {
        id: format!("{caller_qn}>{name}"),
        caller_id: symbol_id(path, caller_qn, kind::METHOD),
        file_path: path.into(),
        name: name.into(),
        receiver: "self".into(),
        line: 5,
    }
}

fn base_input() -> ResolveInput {
    let p = "svc/mail.py";
    ResolveInput {
        files: vec![file(p)],
        symbols: vec![
            sym(p, "svc.mail", kind::MODULE),
            sym(p, "svc.mail.Root", kind::CLASS),
            sym(p, "svc.mail.Root.envelope", kind::METHOD),
            sym(p, "svc.mail.Base", kind::CLASS),
            sym(p, "svc.mail.Smtp", kind::CLASS),
            sym(p, "svc.mail.Smtp.send", kind::METHOD),
        ],
        calls: vec![self_call(p, "svc.mail.Smtp.send", "envelope")],
        facts: vec![
            inherits(p, "svc.mail.Smtp", "Base"),
            inherits(p, "svc.mail.Base", "Root"),
        ],
        ..ResolveInput::default()
    }
}

#[test]
fn a_method_of_a_base_class_two_levels_up_is_a_static_call() {
    let out = resolve(&base_input());
    let p = "svc/mail.py";
    let call = out
        .edges
        .iter()
        .find(|e| e.edge_type == EdgeType::Calls)
        .expect("a CALLS edge");
    assert_eq!(call.from, symbol_id(p, "svc.mail.Smtp.send", kind::METHOD));
    assert_eq!(call.to, symbol_id(p, "svc.mail.Root.envelope", kind::METHOD));
    assert_eq!((call.method.as_str(), call.confidence), (method::STATIC, 0.90));
}

#[test]
fn two_bases_that_both_define_the_method_are_not_guessed() {
    let p = "svc/mail.py";
    let mut input = base_input();
    input.symbols.push(sym(p, "svc.mail.Mixin", kind::CLASS));
    input.symbols.push(sym(p, "svc.mail.Mixin.envelope", kind::METHOD));
    input.facts.push(inherits(p, "svc.mail.Smtp", "Mixin"));
    let out = resolve(&input);
    let call = out
        .edges
        .iter()
        .find(|e| e.edge_type == EdgeType::Calls)
        .expect("a CALLS edge");
    assert_eq!(call.method, method::UNRESOLVED, "{call:?}");
}

#[test]
fn a_cycle_of_bases_ends_and_an_unknown_base_changes_nothing() {
    let p = "svc/mail.py";
    let mut input = base_input();
    input.symbols.retain(|s| s.qualified_name != "svc.mail.Root.envelope");
    input.facts.push(inherits(p, "svc.mail.Root", "Smtp"));
    input.facts.push(inherits(p, "svc.mail.Smtp", "NotInTheRepository"));
    let out = resolve(&input);
    assert!(
        out.edges.iter().all(|e| e.edge_type != EdgeType::Calls),
        "no target exists: {:?}",
        out.edges
    );
    assert_eq!(out.stats.calls_external, 1);
}
