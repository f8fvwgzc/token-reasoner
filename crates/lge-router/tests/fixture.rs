//! End-to-end: index the fixture services, then check edges, impact and routing.
use lge_router::{route, Level, RouterConfig};
use lge_store::Store;
use std::path::Path;

fn build() -> (Store, std::path::PathBuf) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/services");
    let db = std::env::temp_dir().join(format!("lge_test_{}_{:?}.lbug", std::process::id(), std::thread::current().id()));
    let _ = std::fs::remove_file(&db);
    let store = Store::open(&db).unwrap();
    for f in lge_index::scan(&root, 1).unwrap() {
        let (bytes, hash) = lge_index::hash_file(&f.abs_path).unwrap();
        let parsed = lge_index::parse_python(&f.rel_path, &f.service, &bytes, &hash).unwrap();
        store.insert_parsed(&parsed, 0).unwrap();
    }
    store.rebuild_edges().unwrap();
    (store, db)
}

fn id(store: &Store, qn: &str) -> String {
    store.find(qn).unwrap().into_iter().next().unwrap_or_else(|| panic!("missing {qn}")).id
}

#[test]
fn graph_impact_and_routing() {
    let (store, db) = build();

    // Call graph: handler -> service -> repository, with tests attached.
    let save = id(&store, "work_svc.repository.TaskRepository.save_task");
    let callers: Vec<String> = store.callers(&save, 3).unwrap().into_iter().map(|h| h.qualified_name).collect();
    assert!(callers.contains(&"work_svc.service.TaskService.create_task".to_string()), "{callers:?}");
    assert!(callers.contains(&"work_svc.controller.create_task_handler".to_string()), "{callers:?}");
    assert!(callers.contains(&"work_svc.tests.test_service.test_create_task".to_string()), "{callers:?}");

    // Cross-service edge through an import.
    let verify = id(&store, "auth_svc.tokens.verify_token");
    let callers: Vec<String> = store.callers(&verify, 1).unwrap().into_iter().map(|h| h.qualified_name).collect();
    assert_eq!(callers, vec!["work_svc.controller.create_task_handler".to_string()]);

    // Module alias call: util.slugify(...)
    let slug = id(&store, "shared.util.slugify");
    assert_eq!(store.callers(&slug, 1).unwrap().len(), 1);

    // Search finds the handler for a task-creation request.
    let hits = store.search("which function handles a task creation request", 5).unwrap();
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
    store.rebuild_edges().unwrap();
    assert_eq!(before, store.stats().unwrap());

    drop(store);
    let _ = std::fs::remove_file(&db);
}
