//! Files that are not code but that tasks touch: HTML pages, stylesheets,
//! Markdown, SQL schemas and configuration. Each becomes a `document` symbol
//! with a short outline as its signature, plus one `section` symbol per
//! heading, element id, selector, table or top-level key, with its line range.
//! A task such as "make the checkout button blue" then finds `#checkout` in
//! `index.html` at its exact lines, and a change to the file is re-indexed
//! like any source file.
//!
//! The outline is read from the text with simple line rules, not a parser: it
//! only has to say where things are, and it never produces edges.

use crate::{cap_body, unique_ids};
use anyhow::Result;
use lge_core::{file_id, kind, symbol_id, FileRec, ParsedFile, SymbolRec};

pub const HTML: &str = "html";
pub const CSS: &str = "css";
pub const MARKDOWN: &str = "markdown";
pub const SQL: &str = "sql";
pub const YAML: &str = "yaml";
pub const TOML: &str = "toml";
pub const JSON: &str = "json";

/// Larger documents keep only their file symbol: they are data, not text a
/// task edits by hand.
const MAX_OUTLINED_BYTES: usize = 512 * 1024;
/// Sections per file at most.
const MAX_SECTIONS: usize = 300;

/// The document type of an extension.
pub fn lang_of_extension(ext: &str) -> Option<&'static str> {
    Some(match ext.to_ascii_lowercase().as_str() {
        "html" | "htm" | "vue" | "svelte" => HTML,
        "css" | "scss" | "sass" | "less" => CSS,
        "md" | "markdown" | "mdx" => MARKDOWN,
        "sql" => SQL,
        "yaml" | "yml" => YAML,
        "toml" => TOML,
        "json" => JSON,
        _ => return None,
    })
}

pub fn is_document(lang: &str) -> bool {
    matches!(lang, HTML | CSS | MARKDOWN | SQL | YAML | TOML | JSON)
}

/// Lock files and minified files are generated: never indexed.
pub fn is_generated(file_name: &str) -> bool {
    matches!(
        file_name,
        "package-lock.json"
            | "composer.lock"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "Cargo.lock"
            | "poetry.lock"
            | "Gemfile.lock"
    ) || file_name.ends_with(".min.css")
        || file_name.ends_with(".map")
}

pub fn comment_token(lang: &str) -> Option<&'static str> {
    Some(match lang {
        HTML | MARKDOWN => "<!--",
        CSS => "/*",
        SQL => "--",
        YAML | TOML => "#",
        JSON => "//",
        _ => return None,
    })
}

/// A heading, id, selector, table or key found in the text.
struct Section {
    name: String,
    /// 1-based.
    line: usize,
    /// Markdown heading level; 0 for kinds without levels.
    level: usize,
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    for quote in ['"', '\''] {
        let open = format!("{name}={quote}");
        if let Some(i) = tag.find(&open) {
            let rest = &tag[i + open.len()..];
            return rest.find(quote).map(|j| &rest[..j]);
        }
    }
    None
}

/// Text between `<tag ...>` and `</tag>` on one line, tags stripped.
fn inner_text(line: &str, tag: &str) -> Option<String> {
    let start = line.find(&format!("<{tag}"))?;
    let open_end = line[start..].find('>')? + start + 1;
    let close = line[open_end..]
        .find(&format!("</{tag}"))
        .map(|i| i + open_end)
        .unwrap_or(line.len());
    let mut text = String::new();
    let mut in_tag = false;
    for c in line[open_end..close].chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

fn html(lines: &[&str]) -> (Vec<Section>, String, Vec<String>) {
    let mut sections = Vec::new();
    let mut title = String::new();
    let mut links = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let lower = line.to_ascii_lowercase();
        if title.is_empty() {
            if let Some(t) = inner_text(&lower, "title")
                .and_then(|_| inner_text(line, "title").or_else(|| inner_text(line, "TITLE")))
            {
                title = t;
            }
        }
        for level in 1..=3 {
            let tag = format!("h{level}");
            if lower.contains(&format!("<{tag}")) {
                if let Some(text) = inner_text(line, &tag) {
                    sections.push(Section {
                        name: text,
                        line: i + 1,
                        level: 0,
                    });
                }
            }
        }
        // Every element with an id on this line.
        let mut rest = *line;
        while let Some(at) = rest.find(" id=") {
            let tag_part = &rest[at..];
            if let Some(id) = attr(tag_part, " id") {
                if !id.is_empty() && !id.contains("{{") {
                    sections.push(Section {
                        name: format!("#{id}"),
                        line: i + 1,
                        level: 0,
                    });
                }
            }
            rest = &rest[at + 4..];
        }
        for (tag, name) in [("<script", "src"), ("<link", "href")] {
            if lower.contains(tag) {
                if let Some(target) = attr(line, &format!(" {name}")) {
                    if !target.starts_with("http") && !target.starts_with("//") {
                        links.push(target.to_string());
                    }
                }
            }
        }
    }
    (sections, title, links)
}

fn markdown(lines: &[&str]) -> (Vec<Section>, String) {
    let mut sections = Vec::new();
    let mut in_code = false;
    for (i, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        let level = line.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&level) && line[level..].starts_with(' ') {
            sections.push(Section {
                name: line[level..].trim().to_string(),
                line: i + 1,
                level,
            });
        }
    }
    let title = sections
        .iter()
        .find(|s| s.level == 1)
        .or(sections.first())
        .map(|s| s.name.clone())
        .unwrap_or_default();
    (sections, title)
}

fn css(lines: &[&str]) -> Vec<Section> {
    let mut sections = Vec::new();
    let mut depth = 0usize;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if depth == 0 && t.ends_with('{') && !t.starts_with("/*") {
            let selector = t.trim_end_matches('{').trim();
            if !selector.is_empty() {
                sections.push(Section {
                    name: selector.chars().take(120).collect(),
                    line: i + 1,
                    level: 0,
                });
            }
        }
        depth += t.matches('{').count();
        depth = depth.saturating_sub(t.matches('}').count());
    }
    sections
}

fn sql(lines: &[&str]) -> Vec<Section> {
    let mut sections = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let upper: Vec<String> = words.iter().map(|w| w.to_ascii_uppercase()).collect();
        if upper.first().map(String::as_str) != Some("CREATE") {
            continue;
        }
        // CREATE [OR REPLACE] [TEMP] TABLE|VIEW|FUNCTION|PROCEDURE|INDEX|TYPE [IF NOT EXISTS] name
        let Some(k) = upper.iter().position(|w| {
            matches!(
                w.as_str(),
                "TABLE" | "VIEW" | "FUNCTION" | "PROCEDURE" | "INDEX" | "TYPE" | "TRIGGER"
            )
        }) else {
            continue;
        };
        let mut j = k + 1;
        while upper
            .get(j)
            .is_some_and(|w| matches!(w.as_str(), "IF" | "NOT" | "EXISTS"))
        {
            j += 1;
        }
        if let Some(name) = words.get(j) {
            let name = name
                .trim_matches(|c: char| c == '(' || c == ';' || c == '"' || c == '`')
                .split('(')
                .next()
                .unwrap_or("");
            if !name.is_empty() {
                sections.push(Section {
                    name: format!("{} {name}", upper[k].to_lowercase()),
                    line: i + 1,
                    level: 0,
                });
            }
        }
    }
    sections
}

fn keys(lines: &[&str], lang: &str) -> Vec<Section> {
    let mut sections = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let name = match lang {
            // [table] and [[array.of.tables]]
            TOML if line.starts_with('[') => line.trim().trim_matches(['[', ']']).to_string(),
            TOML => continue,
            // Top-level keys only: no indentation.
            YAML if !line.starts_with([' ', '\t', '-', '#']) && line.contains(':') => {
                line.split(':').next().unwrap_or("").trim().to_string()
            }
            YAML => continue,
            // "key": at the first nesting level, two-space or tab indented.
            JSON => {
                let t = line.trim_start();
                let indent = line.len() - t.len();
                if !(1..=4).contains(&indent) || !t.starts_with('"') {
                    continue;
                }
                match t[1..].split_once('"') {
                    Some((k, rest)) if rest.trim_start().starts_with(':') => k.to_string(),
                    _ => continue,
                }
            }
            _ => continue,
        };
        if !name.is_empty() {
            sections.push(Section {
                name,
                line: i + 1,
                level: 0,
            });
        }
    }
    sections
}

/// Outline one document file.
pub fn parse_document(rel_path: &str, service: &str, src: &[u8], hash: &str, lang: &'static str) -> Result<ParsedFile> {
    let text = String::from_utf8_lossy(src);
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len().max(1) as i64;
    let module = rel_path
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(rel_path)
        .replace(['/', '\\'], ".");
    let file_name = rel_path.rsplit('/').next().unwrap_or(rel_path).to_string();
    let mut out = ParsedFile {
        file: Some(FileRec {
            id: file_id(rel_path),
            path: rel_path.to_string(),
            service: service.to_string(),
            lang: lang.to_string(),
            content_hash: hash.to_string(),
        }),
        ..ParsedFile::default()
    };
    let symbol =
        |name: &str, qn: &str, k: &str, signature: String, doc: String, start: i64, end: i64, body: String| SymbolRec {
            id: symbol_id(rel_path, qn, k),
            name: name.to_string(),
            qualified_name: qn.to_string(),
            kind: k.to_string(),
            signature,
            doc,
            body,
            file_path: rel_path.to_string(),
            service: service.to_string(),
            start_line: start,
            end_line: end,
            is_test: false,
        };
    out.symbols.push(symbol(
        &file_name,
        &module,
        kind::MODULE,
        String::new(),
        String::new(),
        1,
        total,
        String::new(),
    ));

    let (mut sections, title, links) = if src.len() > MAX_OUTLINED_BYTES {
        (Vec::new(), String::new(), Vec::new())
    } else {
        match lang {
            HTML => html(&lines),
            MARKDOWN => {
                let (s, t) = markdown(&lines);
                (s, t, Vec::new())
            }
            CSS => (css(&lines), String::new(), Vec::new()),
            SQL => (sql(&lines), String::new(), Vec::new()),
            _ => (keys(&lines, lang), String::new(), Vec::new()),
        }
    };
    sections.truncate(MAX_SECTIONS);

    // The document itself: what it is called and what is in it, at a glance.
    let names: Vec<&str> = sections.iter().map(|s| s.name.as_str()).collect();
    let mut outline = names.join(", ");
    if !links.is_empty() {
        outline = format!("{outline}; uses {}", links.join(", "));
    }
    let outline: String = outline.trim_start_matches("; ").chars().take(240).collect();
    let doc: String = title.chars().take(200).collect();
    let document_qn = format!("{module}.{file_name}");
    out.symbols.push(symbol(
        &file_name,
        &document_qn,
        kind::DOCUMENT,
        outline,
        doc,
        1,
        total,
        cap_body(src),
    ));

    // Each section runs to the line before the next section of the same or a
    // higher level (Markdown), or before the next section (everything else).
    for (i, s) in sections.iter().enumerate() {
        let next = sections[i + 1..]
            .iter()
            .find(|n| s.level == 0 || n.level <= s.level)
            .map(|n| n.line - 1);
        let end = next.unwrap_or(lines.len()).max(s.line);
        let body = lines[s.line - 1..end].join("\n");
        let qn = format!("{document_qn}.{}", s.name);
        out.symbols.push(symbol(
            &s.name,
            &qn,
            kind::SECTION,
            String::new(),
            String::new(),
            s.line as i64,
            end as i64,
            cap_body(body.as_bytes()),
        ));
    }
    unique_ids(&mut out);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outline(path: &str, src: &str) -> Vec<(String, String, i64, i64)> {
        let lang = lang_of_extension(path.rsplit('.').next().unwrap()).unwrap();
        let p = parse_document(path, "svc", src.as_bytes(), "h", lang).unwrap();
        p.symbols
            .into_iter()
            .map(|s| (s.kind, s.name, s.start_line, s.end_line))
            .collect()
    }

    #[test]
    fn an_html_page_has_its_title_ids_headings_and_links() {
        let src = "<!doctype html>\n<html><head><title>Demo shop</title>\n<link rel=\"stylesheet\" href=\"style.css\"></head>\n<body>\n<h1>Demo shop</h1>\n<button id=\"checkout\" class=\"big\">Checkout</button>\n<div id='cart'></div>\n<script src=\"app.js\"></script>\n</body></html>\n";
        let p = parse_document("web/index.html", "web", src.as_bytes(), "h", HTML).unwrap();
        let doc = p.symbols.iter().find(|s| s.kind == kind::DOCUMENT).unwrap();
        assert_eq!((doc.name.as_str(), doc.doc.as_str()), ("index.html", "Demo shop"));
        assert_eq!(doc.signature, "Demo shop, #checkout, #cart; uses style.css, app.js");
        assert_eq!(doc.qualified_name, "web.index.index.html");
        let checkout = p.symbols.iter().find(|s| s.name == "#checkout").unwrap();
        assert_eq!((checkout.kind.as_str(), checkout.start_line), (kind::SECTION, 6));
        assert!(checkout.body.contains("<button id=\"checkout\""));
    }

    #[test]
    fn markdown_sections_run_to_the_next_heading_of_their_level() {
        let src = "# Guide\nintro\n## Install\nsteps\n```\n# not a heading\n```\n## Use\nmore\n# Other\n";
        let o = outline("docs/guide.md", src);
        assert!(o.contains(&("section".into(), "Guide".into(), 1, 9)), "{o:?}");
        assert!(o.contains(&("section".into(), "Install".into(), 3, 7)), "{o:?}");
        assert!(o.contains(&("section".into(), "Use".into(), 8, 9)), "{o:?}");
        assert!(!o.iter().any(|s| s.1 == "not a heading"));
    }

    #[test]
    fn css_sql_and_config_files_list_what_they_define() {
        let css =
            ".btn {\n  color: red;\n}\n@media (max-width: 600px) {\n  .btn { color: blue; }\n}\n#checkout:hover {\n}\n";
        let names: Vec<String> = outline("web/style.css", css)
            .into_iter()
            .filter(|s| s.0 == "section")
            .map(|s| s.1)
            .collect();
        assert_eq!(names, [".btn", "@media (max-width: 600px)", "#checkout:hover"]);

        let sql = "CREATE TABLE IF NOT EXISTS orders (\n  id int\n);\ncreate view open_orders as select 1;\nCREATE OR REPLACE FUNCTION total(x int) returns int;\n";
        let names: Vec<String> = outline("db/schema.sql", sql)
            .into_iter()
            .filter(|s| s.0 == "section")
            .map(|s| s.1)
            .collect();
        assert_eq!(names, ["table orders", "view open_orders", "function total"]);

        let yaml = "name: shop\nservices:\n  web:\n    image: x\n# comment\nversion: 3\n";
        let names: Vec<String> = outline("docker-compose.yml", yaml)
            .into_iter()
            .filter(|s| s.0 == "section")
            .map(|s| s.1)
            .collect();
        assert_eq!(names, ["name", "services", "version"]);

        let toml = "[package]\nname = \"x\"\n[[bin]]\nname = \"y\"\n";
        let names: Vec<String> = outline("Cargo.toml", toml)
            .into_iter()
            .filter(|s| s.0 == "section")
            .map(|s| s.1)
            .collect();
        assert_eq!(names, ["package", "bin"]);

        let json = "{\n  \"name\": \"shop\",\n  \"scripts\": {\n    \"test\": \"jest\"\n  }\n}\n";
        let names: Vec<String> = outline("package.json", json)
            .into_iter()
            .filter(|s| s.0 == "section")
            .map(|s| s.1)
            .collect();
        assert_eq!(names, ["name", "scripts", "test"]);
    }

    #[test]
    fn large_and_generated_files_are_not_outlined() {
        let big = "x: 1\n".repeat(200_000);
        let p = parse_document("data.yaml", "s", big.as_bytes(), "h", YAML).unwrap();
        assert_eq!(p.symbols.iter().filter(|s| s.kind == kind::SECTION).count(), 0);
        assert!(is_generated("package-lock.json") && is_generated("app.min.css") && !is_generated("package.json"));
        assert_eq!(lang_of_extension("HTML"), Some(HTML));
        assert_eq!(lang_of_extension("rs"), None);
    }
}
