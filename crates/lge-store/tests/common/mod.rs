//! Hand-built records for the store tests. Nothing here depends on the indexer
//! or the resolver: the store is tested against the shapes of `lge-core` only.
//!
//! The fixture is a small copy of `tests/fixtures/polyglot`: a Python service
//! (`orders_api`) and two TypeScript services (`web_gateway`, `notifier`),
//! with one edge of every type and every provenance.
#![allow(dead_code)]

use lge_core::{
    confidence, endpoint_id, fact, file_id, kind, lang, method, queue_id, service_id, symbol_id, table_id,
    unresolved_id, CallSite, EdgeRec, EdgeType, FactSite, FileRec, ImportSite, NodeLabel, NodeRec, ParsedFile,
    Resolved, SymbolRec,
};
use lge_store::{GraphStore, Store};
use tempfile::TempDir;

pub const COMMIT: &str = "c0ffee1";
pub const NOW: i64 = 1_800_000_000;

/// A store on its own temporary database path. Keep the `TempDir` alive.
pub fn open() -> (TempDir, Store) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::open(&dir.path().join("graph.lbug")).expect("open store");
    (dir, store)
}

pub fn file(path: &str, service: &str, language: &str) -> FileRec {
    FileRec {
        id: file_id(path),
        path: path.to_string(),
        service: service.to_string(),
        lang: language.to_string(),
        content_hash: lge_core::content_hash(path.as_bytes()),
    }
}

/// A symbol whose name is the last segment of its qualified name.
pub fn sym(f: &FileRec, qualified_name: &str, kind: &str, start_line: i64, end_line: i64) -> SymbolRec {
    let name = qualified_name.rsplit('.').next().unwrap_or(qualified_name).to_string();
    let is_module = kind == kind::MODULE;
    SymbolRec {
        id: symbol_id(&f.path, qualified_name, kind),
        signature: if is_module {
            String::new()
        } else {
            format!("{kind} {name}(...)")
        },
        doc: String::new(),
        body: if is_module {
            String::new()
        } else {
            format!("{kind} {name} {{\n  // body of {qualified_name}\n}}")
        },
        name,
        qualified_name: qualified_name.to_string(),
        kind: kind.to_string(),
        file_path: f.path.clone(),
        service: f.service.clone(),
        start_line,
        end_line,
        is_test: false,
    }
}

pub fn call(caller: &SymbolRec, name: &str, receiver: &str, line: i64) -> CallSite {
    CallSite {
        id: lge_core::stable_id(&["call", &caller.id, name, receiver, &line.to_string()]),
        caller_id: caller.id.clone(),
        file_path: caller.file_path.clone(),
        name: name.to_string(),
        receiver: receiver.to_string(),
        line,
    }
}

pub fn import(f: &FileRec, module: &str, name: &str, alias: &str) -> ImportSite {
    ImportSite {
        id: lge_core::stable_id(&["import", &f.path, module, name, alias]),
        file_path: f.path.clone(),
        module: module.to_string(),
        name: name.to_string(),
        alias: alias.to_string(),
    }
}

pub fn fact_site(owner: &SymbolRec, kind: &str, key: &str, detail: &str, line: i64, literal: bool) -> FactSite {
    FactSite {
        id: lge_core::stable_id(&["fact", &owner.id, kind, key, detail, &line.to_string()]),
        symbol_id: owner.id.clone(),
        file_path: owner.file_path.clone(),
        kind: kind.to_string(),
        key: key.to_string(),
        detail: detail.to_string(),
        line,
        end_line: line + 1,
        literal,
    }
}

pub fn parsed(file: FileRec, symbols: Vec<SymbolRec>) -> ParsedFile {
    ParsedFile {
        file: Some(file),
        symbols,
        ..ParsedFile::default()
    }
}

/// The parsed files of the fixture, by path.
pub struct Fixture {
    pub files: Vec<ParsedFile>,
}

impl Fixture {
    pub fn get(&self, path: &str) -> &ParsedFile {
        self.files
            .iter()
            .find(|p| p.file.as_ref().is_some_and(|f| f.path == path))
            .unwrap_or_else(|| panic!("no fixture file {path}"))
    }

    /// Id of the symbol with this qualified name.
    pub fn id(&self, qualified_name: &str) -> String {
        self.symbol(qualified_name).id.clone()
    }

    pub fn symbol(&self, qualified_name: &str) -> &SymbolRec {
        self.files
            .iter()
            .flat_map(|p| p.symbols.iter())
            .find(|s| s.qualified_name == qualified_name)
            .unwrap_or_else(|| panic!("no fixture symbol {qualified_name}"))
    }

    pub fn symbol_count(&self) -> usize {
        self.files.iter().map(|p| p.symbols.len()).sum()
    }
}

pub const ROUTES_PY: &str = "orders_api/routes.py";
pub const REPO_PY: &str = "orders_api/repo.py";
pub const EVENTS_PY: &str = "orders_api/events.py";
pub const TEST_PY: &str = "orders_api/tests/test_routes.py";
pub const ROUTES_TS: &str = "web_gateway/src/routes.ts";
pub const CLIENT_TS: &str = "web_gateway/src/ordersClient.ts";
pub const VIEWS_TS: &str = "web_gateway/src/views.ts";
pub const CONSUMER_TS: &str = "notifier/src/consumer.ts";
pub const MAILER_TS: &str = "notifier/src/mailer.ts";
pub const TEMPLATES_TS: &str = "notifier/src/templates.ts";
pub const AUDIT_TS: &str = "notifier/src/audit.ts";

pub fn fixture() -> Fixture {
    let mut files = Vec::new();

    // ---- orders_api (Python) ------------------------------------------------
    let f = file(ROUTES_PY, "orders_api", lang::PYTHON);
    let module = sym(&f, "orders_api.routes", kind::MODULE, 1, 20);
    let mut create_order = sym(&f, "orders_api.routes.create_order", kind::FUNCTION, 5, 12);
    create_order.signature = "def create_order(payload)".into();
    create_order.doc = "Create an order and announce it on the queue.".into();
    create_order.body =
        "def create_order(payload):\n    order = save_order(payload)\n    publish_created(order)\n".into();
    let mut get_order = sym(&f, "orders_api.routes.get_order", kind::FUNCTION, 14, 20);
    get_order.signature = "def get_order(order_id)".into();
    get_order.doc = "Return one order.".into();
    files.push(ParsedFile {
        calls: vec![
            call(&create_order, "save_order", "", 8),
            call(&create_order, "publish_created", "", 9),
            call(&get_order, "load_order", "", 16),
        ],
        imports: vec![
            import(&f, ".repo", "save_order", "save_order"),
            import(&f, ".events", "publish_created", "publish_created"),
        ],
        facts: vec![
            fact_site(&create_order, fact::API_EXPOSE, "/orders", "POST", 4, true),
            fact_site(&get_order, fact::API_EXPOSE, "/orders/{order_id}", "GET", 13, true),
        ],
        file: Some(f),
        symbols: vec![module, create_order, get_order],
    });

    let f = file(REPO_PY, "orders_api", lang::PYTHON);
    let module = sym(&f, "orders_api.repo", kind::MODULE, 1, 14);
    let mut save_order = sym(&f, "orders_api.repo.save_order", kind::FUNCTION, 3, 7);
    save_order.doc = "Persist the order row.".into();
    let load_order = sym(&f, "orders_api.repo.load_order", kind::FUNCTION, 9, 14);
    files.push(ParsedFile {
        facts: vec![
            fact_site(&save_order, fact::DB_WRITE, "orders", "", 5, true),
            fact_site(&load_order, fact::DB_READ, "orders", "", 11, true),
        ],
        file: Some(f),
        symbols: vec![module, save_order, load_order],
        ..ParsedFile::default()
    });

    let f = file(EVENTS_PY, "orders_api", lang::PYTHON);
    let module = sym(&f, "orders_api.events", kind::MODULE, 1, 9);
    let publish_created = sym(&f, "orders_api.events.publish_created", kind::FUNCTION, 4, 9);
    files.push(ParsedFile {
        facts: vec![fact_site(
            &publish_created,
            fact::QUEUE_PUBLISH,
            "order.created",
            "queue",
            7,
            true,
        )],
        file: Some(f),
        symbols: vec![module, publish_created],
        ..ParsedFile::default()
    });

    let f = file(TEST_PY, "orders_api", lang::PYTHON);
    let mut module = sym(&f, "orders_api.tests.test_routes", kind::MODULE, 1, 8);
    module.is_test = true;
    let mut test_create_order = sym(
        &f,
        "orders_api.tests.test_routes.test_create_order",
        kind::FUNCTION,
        4,
        8,
    );
    test_create_order.is_test = true;
    files.push(ParsedFile {
        calls: vec![call(&test_create_order, "create_order", "", 6)],
        imports: vec![import(&f, "orders_api.routes", "create_order", "create_order")],
        file: Some(f),
        symbols: vec![module, test_create_order],
        ..ParsedFile::default()
    });

    // ---- web_gateway (TypeScript) --------------------------------------------
    let f = file(ROUTES_TS, "web_gateway", lang::TYPESCRIPT);
    let module = sym(&f, "web_gateway.src.routes", kind::MODULE, 1, 30);
    let get_handler = sym(&f, "web_gateway.src.routes.getOrderHandler", kind::FUNCTION, 5, 12);
    let create_handler = sym(&f, "web_gateway.src.routes.createOrderHandler", kind::FUNCTION, 14, 22);
    files.push(ParsedFile {
        calls: vec![
            call(&get_handler, "fetchOrder", "", 7),
            call(&create_handler, "postOrder", "", 16),
        ],
        imports: vec![
            import(&f, "./ordersClient", "fetchOrder", "fetchOrder"),
            import(&f, "express", "default", "express"),
        ],
        facts: vec![
            fact_site(&get_handler, fact::API_EXPOSE, "/api/orders/:id", "GET", 25, true),
            fact_site(&create_handler, fact::REFERENCE, "render", "", 20, true),
        ],
        file: Some(f),
        symbols: vec![module, get_handler, create_handler],
    });

    let f = file(CLIENT_TS, "web_gateway", lang::TYPESCRIPT);
    let module = sym(&f, "web_gateway.src.ordersClient", kind::MODULE, 1, 20);
    let fetch_order = sym(&f, "web_gateway.src.ordersClient.fetchOrder", kind::FUNCTION, 4, 8);
    let post_order = sym(&f, "web_gateway.src.ordersClient.postOrder", kind::FUNCTION, 10, 14);
    let probe_order = sym(&f, "web_gateway.src.ordersClient.probe", kind::FUNCTION, 16, 20);
    files.push(ParsedFile {
        facts: vec![
            fact_site(
                &fetch_order,
                fact::API_CALL,
                "${ORDERS_URL}/orders/${id}",
                "GET",
                6,
                false,
            ),
            fact_site(
                &post_order,
                fact::API_CALL,
                "http://orders-api/orders",
                "POST",
                12,
                true,
            ),
        ],
        file: Some(f),
        symbols: vec![module, fetch_order, post_order, probe_order],
        ..ParsedFile::default()
    });

    let f = file(VIEWS_TS, "web_gateway", lang::TYPESCRIPT);
    let module = sym(&f, "web_gateway.src.views", kind::MODULE, 1, 9);
    let render = sym(&f, "web_gateway.src.views.render", kind::FUNCTION, 3, 9);
    files.push(parsed(f, vec![module, render]));

    // ---- notifier (TypeScript) -----------------------------------------------
    let f = file(CONSUMER_TS, "notifier", lang::TYPESCRIPT);
    let module = sym(&f, "notifier.src.consumer", kind::MODULE, 1, 30);
    let mut handle = sym(&f, "notifier.src.consumer.handleOrderCreated", kind::FUNCTION, 6, 24);
    handle.doc = "Send the confirmation mail for a new order.".into();
    files.push(ParsedFile {
        calls: vec![
            call(&handle, "sendEmail", "", 10),
            call(&handle, "recordDelivery", "audit", 14),
            call(&handle, "render", "tpl", 18),
        ],
        imports: vec![import(&f, "./mailer", "sendEmail", "sendEmail")],
        facts: vec![
            fact_site(&handle, fact::QUEUE_CONSUME, "order.created", "queue", 28, true),
            fact_site(&handle, fact::DB_READ, "customers", "", 12, true),
        ],
        file: Some(f),
        symbols: vec![module, handle],
    });

    let f = file(MAILER_TS, "notifier", lang::TYPESCRIPT);
    let module = sym(&f, "notifier.src.mailer", kind::MODULE, 1, 40);
    let mailer = sym(&f, "notifier.src.mailer.Mailer", kind::INTERFACE, 3, 5);
    let base = sym(&f, "notifier.src.mailer.BaseMailer", kind::CLASS, 7, 12);
    let smtp = sym(&f, "notifier.src.mailer.SmtpMailer", kind::CLASS, 14, 30);
    let send_method = sym(&f, "notifier.src.mailer.SmtpMailer.send", kind::METHOD, 16, 28);
    let send_email = sym(&f, "notifier.src.mailer.sendEmail", kind::FUNCTION, 32, 40);
    files.push(ParsedFile {
        facts: vec![
            fact_site(&smtp, fact::INHERITS, "BaseMailer", "", 14, true),
            fact_site(&smtp, fact::IMPLEMENTS, "Mailer", "", 14, true),
        ],
        file: Some(f),
        symbols: vec![module, mailer, base, smtp, send_method, send_email],
        ..ParsedFile::default()
    });

    let f = file(TEMPLATES_TS, "notifier", lang::TYPESCRIPT);
    let module = sym(&f, "notifier.src.templates", kind::MODULE, 1, 9);
    let render = sym(&f, "notifier.src.templates.render", kind::FUNCTION, 3, 9);
    files.push(parsed(f, vec![module, render]));

    let f = file(AUDIT_TS, "notifier", lang::TYPESCRIPT);
    let module = sym(&f, "notifier.src.audit", kind::MODULE, 1, 15);
    let audit_log = sym(&f, "notifier.src.audit.AuditLog", kind::CLASS, 3, 15);
    let record = sym(&f, "notifier.src.audit.AuditLog.recordDelivery", kind::METHOD, 5, 13);
    files.push(parsed(f, vec![module, audit_log, record]));

    Fixture { files }
}

#[allow(clippy::too_many_arguments)]
pub fn edge(
    from: &str,
    to: &str,
    edge_type: EdgeType,
    confidence: f64,
    method: &str,
    evidence_file: &str,
    start_line: i64,
    end_line: i64,
) -> EdgeRec {
    EdgeRec {
        from: from.to_string(),
        to: to.to_string(),
        edge_type,
        confidence,
        method: method.to_string(),
        evidence_file: evidence_file.to_string(),
        start_line,
        end_line,
    }
}

pub fn endpoint(http_method: &str, route: &str, service: &str) -> NodeRec {
    let normalized = lge_core::normalize_route(route);
    NodeRec {
        service: service.to_string(),
        ..NodeRec::bare(
            &endpoint_id(http_method, route),
            NodeLabel::ApiEndpoint,
            &format!("{http_method} {normalized}"),
            &normalized,
            http_method,
        )
    }
}

pub fn table(name: &str) -> NodeRec {
    NodeRec::bare(&table_id(name), NodeLabel::DbTable, name, name, "")
}

pub fn queue(name: &str, queue_kind: &str) -> NodeRec {
    NodeRec::bare(&queue_id(name), NodeLabel::Queue, name, name, queue_kind)
}

pub fn placeholder(name: &str) -> NodeRec {
    NodeRec::bare(&unresolved_id(name), NodeLabel::Symbol, name, name, kind::UNRESOLVED)
}

/// Placeholder endpoint for an HTTP call with several candidate routes.
pub fn placeholder_endpoint(name: &str) -> NodeRec {
    NodeRec::bare(
        &unresolved_id(name),
        NodeLabel::ApiEndpoint,
        name,
        "/x",
        kind::UNRESOLVED,
    )
}

/// What a resolver would derive from [`fixture`]: one edge of every type, and
/// every provenance. Built by hand so the store tests do not need the resolver.
pub fn resolved(fx: &Fixture) -> Resolved {
    let s = |qn: &str| fx.id(qn);
    let post_orders = endpoint("POST", "/orders", "orders_api");
    let get_order = endpoint("GET", "/orders/{order_id}", "orders_api");
    let ambiguous = placeholder_endpoint("GET /x");
    let orders = table("orders");
    let customers = table("customers");
    let created = queue("order.created", "queue");
    let render = placeholder("render");
    let (st, inf, unres) = (method::STATIC, method::INFERRED, method::UNRESOLVED);

    let edges = vec![
        // IMPORTS: File -> File
        edge(
            &file_id(ROUTES_PY),
            &file_id(REPO_PY),
            EdgeType::Imports,
            confidence::IMPORTED,
            st,
            ROUTES_PY,
            1,
            1,
        ),
        edge(
            &file_id(ROUTES_PY),
            &file_id(EVENTS_PY),
            EdgeType::Imports,
            confidence::IMPORTED,
            st,
            ROUTES_PY,
            2,
            2,
        ),
        edge(
            &file_id(CONSUMER_TS),
            &file_id(MAILER_TS),
            EdgeType::Imports,
            confidence::IMPORTED,
            st,
            CONSUMER_TS,
            1,
            1,
        ),
        // CALLS: static, inferred and unresolved
        edge(
            &s("orders_api.routes.create_order"),
            &s("orders_api.repo.save_order"),
            EdgeType::Calls,
            0.90,
            st,
            ROUTES_PY,
            8,
            8,
        ),
        edge(
            &s("orders_api.routes.create_order"),
            &s("orders_api.events.publish_created"),
            EdgeType::Calls,
            0.90,
            st,
            ROUTES_PY,
            9,
            9,
        ),
        edge(
            &s("orders_api.routes.get_order"),
            &s("orders_api.repo.load_order"),
            EdgeType::Calls,
            0.50,
            inf,
            ROUTES_PY,
            16,
            16,
        ),
        edge(
            &s("orders_api.tests.test_routes.test_create_order"),
            &s("orders_api.routes.create_order"),
            EdgeType::Calls,
            0.90,
            st,
            TEST_PY,
            6,
            6,
        ),
        edge(
            &s("web_gateway.src.routes.getOrderHandler"),
            &s("web_gateway.src.ordersClient.fetchOrder"),
            EdgeType::Calls,
            0.90,
            st,
            ROUTES_TS,
            7,
            7,
        ),
        edge(
            &s("web_gateway.src.routes.createOrderHandler"),
            &s("web_gateway.src.ordersClient.postOrder"),
            EdgeType::Calls,
            0.50,
            inf,
            ROUTES_TS,
            16,
            16,
        ),
        edge(
            &s("notifier.src.consumer.handleOrderCreated"),
            &s("notifier.src.mailer.sendEmail"),
            EdgeType::Calls,
            0.90,
            st,
            CONSUMER_TS,
            10,
            10,
        ),
        edge(
            &s("notifier.src.consumer.handleOrderCreated"),
            &s("notifier.src.audit.AuditLog.recordDelivery"),
            EdgeType::Calls,
            0.40,
            inf,
            CONSUMER_TS,
            14,
            14,
        ),
        edge(
            &s("notifier.src.consumer.handleOrderCreated"),
            &render.id,
            EdgeType::Calls,
            0.0,
            unres,
            CONSUMER_TS,
            18,
            18,
        ),
        edge(
            &s("notifier.src.mailer.sendEmail"),
            &s("notifier.src.mailer.SmtpMailer.send"),
            EdgeType::Calls,
            0.90,
            st,
            MAILER_TS,
            35,
            35,
        ),
        // INHERITS, IMPLEMENTS, REFERENCES
        edge(
            &s("notifier.src.mailer.SmtpMailer"),
            &s("notifier.src.mailer.BaseMailer"),
            EdgeType::Inherits,
            0.95,
            st,
            MAILER_TS,
            14,
            14,
        ),
        edge(
            &s("notifier.src.mailer.SmtpMailer"),
            &s("notifier.src.mailer.Mailer"),
            EdgeType::Implements,
            0.95,
            st,
            MAILER_TS,
            14,
            15,
        ),
        edge(
            &s("web_gateway.src.routes.createOrderHandler"),
            &s("web_gateway.src.views.render"),
            EdgeType::References,
            0.50,
            inf,
            ROUTES_TS,
            20,
            21,
        ),
        // EXPOSES_API, CALLS_API
        edge(
            &s("orders_api.routes.create_order"),
            &post_orders.id,
            EdgeType::ExposesApi,
            0.90,
            st,
            ROUTES_PY,
            4,
            5,
        ),
        edge(
            &s("orders_api.routes.get_order"),
            &get_order.id,
            EdgeType::ExposesApi,
            0.90,
            st,
            ROUTES_PY,
            13,
            14,
        ),
        edge(
            &s("web_gateway.src.ordersClient.postOrder"),
            &post_orders.id,
            EdgeType::CallsApi,
            0.60,
            inf,
            CLIENT_TS,
            12,
            13,
        ),
        edge(
            &s("web_gateway.src.ordersClient.fetchOrder"),
            &get_order.id,
            EdgeType::CallsApi,
            0.50,
            inf,
            CLIENT_TS,
            6,
            7,
        ),
        edge(
            &s("web_gateway.src.ordersClient.probe"),
            &ambiguous.id,
            EdgeType::CallsApi,
            0.0,
            unres,
            CLIENT_TS,
            18,
            18,
        ),
        // READS_FROM, WRITES_TO
        edge(
            &s("orders_api.repo.load_order"),
            &orders.id,
            EdgeType::ReadsFrom,
            0.90,
            st,
            REPO_PY,
            11,
            12,
        ),
        edge(
            &s("notifier.src.consumer.handleOrderCreated"),
            &customers.id,
            EdgeType::ReadsFrom,
            0.50,
            inf,
            CONSUMER_TS,
            12,
            13,
        ),
        edge(
            &s("orders_api.repo.save_order"),
            &orders.id,
            EdgeType::WritesTo,
            0.90,
            st,
            REPO_PY,
            5,
            6,
        ),
        // PUBLISHES, CONSUMES
        edge(
            &s("orders_api.events.publish_created"),
            &created.id,
            EdgeType::Publishes,
            0.90,
            st,
            EVENTS_PY,
            7,
            8,
        ),
        edge(
            &s("notifier.src.consumer.handleOrderCreated"),
            &created.id,
            EdgeType::Consumes,
            0.90,
            st,
            CONSUMER_TS,
            28,
            29,
        ),
        // TESTED_BY: symbol -> test
        edge(
            &s("orders_api.routes.create_order"),
            &s("orders_api.tests.test_routes.test_create_order"),
            EdgeType::TestedBy,
            0.90,
            st,
            TEST_PY,
            6,
            6,
        ),
        // DEPENDS_ON: Service -> Service
        edge(
            &service_id("web_gateway"),
            &service_id("orders_api"),
            EdgeType::DependsOn,
            0.60,
            inf,
            CLIENT_TS,
            12,
            13,
        ),
        edge(
            &service_id("notifier"),
            &service_id("orders_api"),
            EdgeType::DependsOn,
            0.90,
            st,
            CONSUMER_TS,
            28,
            29,
        ),
    ];

    let mut out = Resolved {
        nodes: vec![post_orders, get_order, ambiguous, orders, customers, created, render],
        edges,
        ..Resolved::default()
    };
    for e in &out.edges {
        out.stats.add(e.edge_type, &e.method);
    }
    out
}

/// Index the fixture and store the hand-built resolution.
pub fn indexed() -> (TempDir, Store, Fixture, Resolved) {
    let (dir, store) = open();
    let fx = fixture();
    store.set_repository("/repo/polyglot", COMMIT).expect("set repository");
    for p in &fx.files {
        store.insert_parsed(p, COMMIT, NOW).expect("insert parsed file");
    }
    let resolved = resolved(&fx);
    store.replace_edges(&resolved, COMMIT, NOW + 5).expect("replace edges");
    (dir, store, fx, resolved)
}

/// Sort key that makes two edge lists comparable.
pub fn sorted(mut edges: Vec<EdgeRec>) -> Vec<EdgeRec> {
    edges.sort_by(|a, b| (a.edge_type, &a.from, &a.to).cmp(&(b.edge_type, &b.from, &b.to)));
    edges
}

/// The node a stored symbol is read back as.
pub fn node_of(s: &SymbolRec) -> NodeRec {
    NodeRec {
        id: s.id.clone(),
        label: NodeLabel::Symbol,
        name: s.name.clone(),
        qualified_name: s.qualified_name.clone(),
        kind: s.kind.clone(),
        signature: s.signature.clone(),
        doc: s.doc.clone(),
        file_path: s.file_path.clone(),
        service: s.service.clone(),
        start_line: s.start_line,
        end_line: s.end_line,
        is_test: s.is_test,
    }
}

pub fn stat(store: &Store, label: &str) -> i64 {
    store
        .stats()
        .expect("stats")
        .into_iter()
        .find(|(l, _)| l == label)
        .map(|(_, n)| n)
        .unwrap_or_else(|| panic!("no stats line {label}"))
}

pub fn ids(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}
