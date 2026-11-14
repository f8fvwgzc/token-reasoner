//! End-to-end: index the fixture services, then check edges, impact and routing.
use lge_core::{method, EdgeType};
use lge_router::{route, Level, RouterConfig};
use lge_store::{GraphStore, Store};
use std::path::Path;

fn build() -> (Store, tempfile::TempDir) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/services");
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("graph.lbug")).unwrap();
    let report = lge_retrieve::index_repo(&store, &root, &lge_retrieve::IndexOptions::default()).unwrap();
    // The numbers the original indexer and resolver produced on this fixture.
    let stats = report.stats.expect("first run resolves edges");
    assert_eq!((report.indexed, report.failed.len()), (8, 0));
    assert_eq!(stats.get(EdgeType::Calls, method::STATIC), 14);
    assert_eq!(stats.get(EdgeType::Calls, method::INFERRED), 0);
    assert_eq!(stats.get(EdgeType::Calls, method::UNRESOLVED), 0);
    assert_eq!(stats.calls_external, 3);
    assert_eq!(stats.get(EdgeType::Imports, method::STATIC), 5);
    assert_eq!(stats.imports_external, 0);
    (store, dir)
}

fn id(store: &Store, qn: &str) -> String {
    store
        .find(qn)
        .unwrap()
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("missing {qn}"))
        .id
}

#[test]
fn graph_impact_and_routing() {
    let (store, _dir) = build();

    // Call graph: handler -> service -> repository, with tests attached.
    let save = id(&store, "work_svc.repository.TaskRepository.save_task");
    let callers: Vec<String> = store
        .callers(&save, 3)
        .unwrap()
        .into_iter()
        .map(|h| h.qualified_name)
        .collect();
    assert!(
        callers.contains(&"work_svc.service.TaskService.create_task".to_string()),
        "{callers:?}"
    );
    assert!(
        callers.contains(&"work_svc.controller.create_task_handler".to_string()),
        "{callers:?}"
    );
    assert!(
        callers.contains(&"work_svc.tests.test_service.test_create_task".to_string()),
        "{callers:?}"
    );

    // Cross-service edge through an import.
    let verify = id(&store, "auth_svc.tokens.verify_token");
    let callers: Vec<String> = store
        .callers(&verify, 1)
        .unwrap()
        .into_iter()
        .map(|h| h.qualified_name)
        .collect();
    assert_eq!(callers, vec!["work_svc.controller.create_task_handler".to_string()]);

    // Module alias call: util.slugify(...)
    let slug = id(&store, "shared.util.slugify");
    assert_eq!(store.callers(&slug, 1).unwrap().len(), 1);

    // Search finds the handler for a task-creation request.
    let hits = store
        .search("which function handles a task creation request", 5)
        .unwrap();
    assert!(hits.iter().any(|h| h.name == "create_task_handler"), "{hits:?}");

    let cfg = RouterConfig::default();
    // Nothing calls rotate_signing_key: one file.
    let r = route(&store, "rotate signing key", &cfg).unwrap();
    assert_eq!(r.level, Level::Low, "{r:?}");
    // find_all is used by the service and the controller: several files, one service.
    let r = route(&store, "find_all should paginate", &cfg).unwrap();
    assert_eq!(r.level, Level::Medium, "{r:?}");
    // verify_token is called from another service.
    let r = route(&store, "verify token must check expiry", &cfg).unwrap();
    assert_eq!(r.level, Level::Top, "{r:?}");
    // Unknown area: safety rule.
    let r = route(&store, "kubernetes autoscaling policy", &cfg).unwrap();
    assert_eq!(r.level, Level::Top);

    // Re-running edge resolution gives the same graph.
    let before = store.stats().unwrap();
    lge_retrieve::resolve_edges(&store, "", 0).unwrap();
    assert_eq!(before, store.stats().unwrap());

    // A second index run finds nothing to do.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/services");
    let again = lge_retrieve::index_repo(&store, &root, &lge_retrieve::IndexOptions::default()).unwrap();
    assert_eq!((again.indexed, again.unchanged, again.removed), (0, 8, 0));
    assert_eq!(before, store.stats().unwrap());
}

#[test]
fn routing_uses_the_full_change_impact() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/polyglot");
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("graph.lbug")).unwrap();
    lge_retrieve::index_repo(&store, &root, &lge_retrieve::IndexOptions::default()).unwrap();
    let cfg = RouterConfig::default();

    // The handler is called from another service through its endpoint.
    let r = route(&store, "create_order must validate the payload", &cfg).unwrap();
    assert_eq!(r.level, Level::Top, "{r:?}");
    assert_eq!(r.services, ["orders_api", "web_gateway"]);
    assert!(r.resources.contains(&"endpoint POST /orders".to_string()), "{r:?}");
    assert!(
        r.tests
            .contains(&"orders_api.tests.test_routes.test_create_order".to_string()),
        "{r:?}"
    );

    // One file, but a call from it has several candidates: one level up.
    let r = route(&store, "handleOrderCreated should retry the email", &cfg).unwrap();
    assert!(r.unresolved_edges > 0 && r.level != Level::Low, "{r:?}");
    assert!(r.reason.contains("unresolved"), "{}", r.reason);

    // Used from one other file of the same service; every edge is verified.
    let r = route(&store, "find_order should return None for a missing row", &cfg).unwrap();
    assert_eq!(
        (r.level, r.files.as_slice()),
        (
            Level::Medium,
            ["orders_api/repo.py".to_string(), "orders_api/routes.py".to_string()].as_slice()
        ),
        "{r:?}"
    );
}
