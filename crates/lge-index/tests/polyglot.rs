//! Parses the Python half of `tests/fixtures/polyglot` and checks what the
//! architecture document promises about it.

use lge_core::{fact, kind, FactSite, ParsedFile, SymbolRec};
use std::path::Path;

/// Every file of `orders_api`, parsed.
fn orders_api() -> Vec<ParsedFile> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/polyglot");
    let files: Vec<_> = lge_index::scan(&root, 1)
        .unwrap()
        .into_iter()
        .filter(|f| f.service == "orders_api")
        .collect();
    let paths: Vec<&str> = files.iter().map(|f| f.rel_path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "orders_api/__init__.py",
            "orders_api/events.py",
            "orders_api/models.py",
            "orders_api/repo.py",
            "orders_api/routes.py",
            "orders_api/tests/__init__.py",
            "orders_api/tests/test_routes.py",
        ]
    );
    assert!(files.iter().all(|f| f.lang == "python"));
    let parsed = lge_index::parse_many(&files);
    assert_eq!(parsed.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), paths);
    parsed
        .into_iter()
        .map(|(path, r)| r.unwrap_or_else(|e| panic!("{path}: {e:#}")))
        .collect()
}

fn symbol<'a>(files: &'a [ParsedFile], qualified_name: &str) -> &'a SymbolRec {
    files
        .iter()
        .flat_map(|f| &f.symbols)
        .find(|s| s.qualified_name == qualified_name)
        .unwrap_or_else(|| panic!("no symbol {qualified_name}"))
}

/// Facts of one kind on one symbol.
fn facts<'a>(files: &'a [ParsedFile], qualified_name: &str, kind: &str) -> Vec<&'a FactSite> {
    let id = &symbol(files, qualified_name).id;
    files
        .iter()
        .flat_map(|f| &f.facts)
        .filter(|f| f.symbol_id == *id && f.kind == kind)
        .collect()
}

/// (key, detail, literal) of the facts of one kind on one symbol.
fn keys(files: &[ParsedFile], qualified_name: &str, kind: &str) -> Vec<(String, String, bool)> {
    facts(files, qualified_name, kind)
        .into_iter()
        .map(|f| (f.key.clone(), f.detail.clone(), f.literal))
        .collect()
}

fn key(k: &str, detail: &str, literal: bool) -> (String, String, bool) {
    (k.to_string(), detail.to_string(), literal)
}

#[test]
fn routes_are_exposed_by_their_handlers() {
    let files = orders_api();
    // `@router.post("")` on `APIRouter(prefix="/orders")`.
    assert_eq!(
        keys(&files, "orders_api.routes.create_order", fact::API_EXPOSE),
        vec![key("/orders", "POST", true)]
    );
    assert_eq!(
        keys(&files, "orders_api.routes.get_order", fact::API_EXPOSE),
        vec![key("/orders/{order_id}", "GET", true)]
    );
    let get = facts(&files, "orders_api.routes.get_order", fact::API_EXPOSE)[0];
    assert_eq!(get.file_path, "orders_api/routes.py");
    assert_eq!((get.line, get.end_line), (18, 18));
    assert_eq!(lge_core::normalize_route(&get.key), "/orders/{}");

    let create = symbol(&files, "orders_api.routes.create_order");
    assert_eq!(create.kind, kind::FUNCTION);
    assert_eq!(create.service, "orders_api");
    assert_eq!(create.doc, "Create an order and announce it.");
    assert_eq!((create.start_line, create.end_line), (10, 15));
    assert!(create
        .body
        .starts_with("@router.post(\"\")\ndef create_order(payload: dict):"));
    assert!(create.body.ends_with("return {\"id\": order_id}"));

    // The whole service exposes exactly these two endpoints and calls none.
    let all = |kind: &str| files.iter().flat_map(|f| &f.facts).filter(|f| f.kind == kind).count();
    assert_eq!(all(fact::API_EXPOSE), 2);
    assert_eq!(all(fact::API_CALL), 0);
}

#[test]
fn handlers_call_the_repository_and_the_publisher() {
    let files = orders_api();
    let create = symbol(&files, "orders_api.routes.create_order");
    let routes = files
        .iter()
        .find(|f| f.file.as_ref().is_some_and(|x| x.path == "orders_api/routes.py"))
        .unwrap();
    let called: Vec<(&str, &str)> = routes
        .calls
        .iter()
        .filter(|c| c.caller_id == create.id)
        .map(|c| (c.name.as_str(), c.receiver.as_str()))
        .collect();
    assert_eq!(called, vec![("insert_order", ""), ("publish_order_created", "")]);
    let imports: Vec<(&str, &str)> = routes
        .imports
        .iter()
        .map(|i| (i.module.as_str(), i.name.as_str()))
        .collect();
    assert!(imports.contains(&(".repo", "insert_order")), "{imports:?}");
    assert!(imports.contains(&(".repo", "find_order")), "{imports:?}");
    assert!(imports.contains(&(".events", "publish_order_created")), "{imports:?}");
}

#[test]
fn repository_reads_and_writes_the_orders_table() {
    let files = orders_api();
    assert_eq!(
        keys(&files, "orders_api.repo.insert_order", fact::DB_WRITE),
        vec![key("orders", "", true)]
    );
    assert_eq!(keys(&files, "orders_api.repo.insert_order", fact::DB_READ), vec![]);
    assert_eq!(
        keys(&files, "orders_api.repo.find_order", fact::DB_READ),
        vec![key("orders", "", true)]
    );
    assert_eq!(keys(&files, "orders_api.repo.find_order", fact::DB_WRITE), vec![]);
    // Docstrings such as "Load one order by id." are not SQL.
    let tables: Vec<&str> = files
        .iter()
        .flat_map(|f| &f.facts)
        .filter(|f| f.kind == fact::DB_READ || f.kind == fact::DB_WRITE)
        .map(|f| f.key.as_str())
        .collect();
    assert_eq!(tables, vec!["orders", "orders"]);
}

#[test]
fn events_publish_to_the_order_created_queue() {
    let files = orders_api();
    // Default exchange, so the routing key is the queue name.
    assert_eq!(
        keys(&files, "orders_api.events.publish_order_created", fact::QUEUE_PUBLISH),
        vec![key("order.created", "queue", true)]
    );
    let publish = facts(&files, "orders_api.events.publish_order_created", fact::QUEUE_PUBLISH)[0];
    assert_eq!((publish.line, publish.end_line), (12, 16));
    assert_eq!(
        files
            .iter()
            .flat_map(|f| &f.facts)
            .filter(|f| f.kind == fact::QUEUE_CONSUME)
            .count(),
        0
    );
}

#[test]
fn customer_model_maps_the_customers_table() {
    let files = orders_api();
    let customer = symbol(&files, "orders_api.models.Customer");
    assert_eq!(customer.kind, kind::CLASS);
    assert_eq!(
        keys(&files, "orders_api.models.Customer", fact::TABLE_MODEL),
        vec![key("customers", "", true)]
    );
    // `Base = declarative_base()` is a variable, not a class to inherit from.
    assert_eq!(keys(&files, "orders_api.models.Customer", fact::INHERITS), vec![]);
    // `session.query(Customer)` mentions the model without calling it.
    assert_eq!(
        keys(&files, "orders_api.repo.find_customer", fact::REFERENCE),
        vec![key("Customer", "", true)]
    );
}

#[test]
fn test_file_symbols_are_tests_and_call_the_handler() {
    let files = orders_api();
    let test = symbol(&files, "orders_api.tests.test_routes.test_create_order");
    assert!(test.is_test);
    assert_eq!(test.kind, kind::FUNCTION);
    assert!(
        symbol(&files, "orders_api.tests.test_routes").is_test,
        "the module of a test file is a test"
    );
    assert!(symbol(&files, "orders_api.tests").is_test);
    assert!(!symbol(&files, "orders_api.routes.create_order").is_test);

    let file = files
        .iter()
        .find(|f| {
            f.file
                .as_ref()
                .is_some_and(|x| x.path == "orders_api/tests/test_routes.py")
        })
        .unwrap();
    let calls: Vec<(&str, &str)> = file
        .calls
        .iter()
        .filter(|c| c.caller_id == test.id)
        .map(|c| (c.name.as_str(), c.receiver.as_str()))
        .collect();
    assert_eq!(calls, vec![("create_order", "")]);
    let imports: Vec<(&str, &str, &str)> = file
        .imports
        .iter()
        .map(|i| (i.module.as_str(), i.name.as_str(), i.alias.as_str()))
        .collect();
    assert_eq!(imports, vec![("orders_api.routes", "create_order", "create_order")]);
}

#[test]
fn every_fact_points_at_a_symbol_of_its_file() {
    for file in orders_api() {
        let path = &file.file.as_ref().unwrap().path;
        for f in &file.facts {
            assert_eq!(&f.file_path, path);
            assert!(file.symbols.iter().any(|s| s.id == f.symbol_id), "{path}: {f:?}");
            assert!(f.line >= 1 && f.end_line >= f.line, "{f:?}");
            assert!(!f.key.is_empty(), "{f:?}");
        }
    }
}
