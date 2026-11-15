//! Behaviours ported from graphify's extractors: calls in Rust macro
//! arguments, the dialect of `.h` and `.m` files, and large files.
use lge_core::kind;
use lge_index::parse_file;

fn calls(path: &str, src: &str) -> Vec<(String, String)> {
    let p = parse_file(path, "svc", src.as_bytes(), "h").unwrap().unwrap();
    let name_of = |id: &str| {
        p.symbols
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.name.clone())
            .unwrap_or_default()
    };
    let mut v: Vec<(String, String)> = p
        .calls
        .iter()
        .map(|c| (name_of(&c.caller_id), c.name.clone()))
        .collect();
    v.sort();
    v
}

#[test]
fn calls_written_inside_rust_macros_are_calls() {
    let src = r#"
fn total(items: &[u32]) -> u32 { items.iter().sum() }
fn tax(x: u32) -> u32 { x / 10 }
fn report(items: &[u32]) -> String {
    format!("{} + {}", total(items), tax(total(items)))
}
#[cfg(test)]
mod tests {
    #[test]
    fn sums() {
        assert_eq!(total(&[]), 0);
        assert!(vec![tax(10)].len() == 1);
    }
}
"#;
    let c = calls("src/lib.rs", src);
    for want in [
        ("report", "total"),
        ("report", "tax"),
        ("sums", "total"),
        ("sums", "tax"),
    ] {
        assert!(
            c.contains(&(want.0.to_string(), want.1.to_string())),
            "{want:?} missing in {c:?}"
        );
    }
}

#[test]
fn attributes_and_definitions_inside_macros_are_not_calls() {
    let src = r#"
#[cfg(all(test, feature = "x"))]
fn helper() {}
macro_rules! make { ($f:ident) => { fn $f() {} }; }
fn build() {
    quote! { fn generated(x: u32) -> u32 { x } };
    make!(made);
}
"#;
    let c = calls("src/gen.rs", src);
    for wrong in ["all", "generated", "f"] {
        assert!(!c.iter().any(|(_, n)| n == wrong), "`{wrong}` is not a call: {c:?}");
    }
}

#[test]
fn headers_and_dot_m_files_are_read_in_their_own_dialect() {
    let lang = |path: &str, src: &str| {
        parse_file(path, "svc", src.as_bytes(), "h")
            .unwrap()
            .map(|p| p.file.unwrap().lang)
    };
    assert_eq!(lang("a/plain.h", "int add(int a, int b);\n").as_deref(), Some("c"));
    assert_eq!(
        lang("a/cart.h", "class Cart {\npublic:\n  int total();\n};\n").as_deref(),
        Some("cpp")
    );
    assert_eq!(
        lang(
            "a/api.h",
            "#ifdef __cplusplus\nextern \"C\" {\n#endif\nint api(void);\n"
        )
        .as_deref(),
        Some("cpp")
    );
    assert_eq!(
        lang(
            "a/View.h",
            "#import <UIKit/UIKit.h>\n@interface View : UIView\n- (void)draw;\n@end\n"
        )
        .as_deref(),
        Some("objc")
    );
    assert_eq!(
        lang(
            "a/View.m",
            "#import \"View.h\"\n@implementation View\n- (void)draw {}\n@end\n"
        )
        .as_deref(),
        Some("objc")
    );
    // MATLAB shares `.m`: it is left out, not parsed as Objective-C.
    assert_eq!(
        lang("a/solve.m", "function x = solve(a, b)\n  x = a \\ b;\nend\n"),
        None
    );

    // The class in a C++ header named .h is found.
    let p = parse_file(
        "a/cart.h",
        "svc",
        b"class Cart {\npublic:\n  int total() { return 0; }\n};\n",
        "h",
    )
    .unwrap()
    .unwrap();
    assert!(
        p.symbols.iter().any(|s| s.kind == kind::CLASS && s.name == "Cart"),
        "{:?}",
        p.symbols
    );
}

#[test]
fn a_large_file_keeps_its_definitions_but_not_its_calls() {
    let mut src = String::from("fn first() -> u32 { second() }\nfn second() -> u32 { 1 }\n");
    let filler = format!("// {}\n", "x".repeat(100));
    while src.len() <= 2 * 1024 * 1024 {
        src.push_str(&filler);
    }
    let p = parse_file("src/big.rs", "svc", src.as_bytes(), "h").unwrap().unwrap();
    assert!(p.symbols.iter().any(|s| s.name == "first") && p.symbols.iter().any(|s| s.name == "second"));
    assert!(p.calls.is_empty(), "{} calls", p.calls.len());
    let small = parse_file(
        "src/small.rs",
        "svc",
        b"fn first() -> u32 { second() }\nfn second() -> u32 { 1 }\n",
        "h",
    )
    .unwrap()
    .unwrap();
    assert_eq!(small.calls.len(), 1);
}
