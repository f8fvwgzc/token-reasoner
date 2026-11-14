//! Parses every file of `tests/fixtures/polyglot` (one Python and two
//! TypeScript services) and checks what the architecture document promises
//! about it: the names are fixed there so that the services connect.

use lge_core::{fact, kind, normalize_route, ParsedFile};
use std::path::Path;

/// Everything extracted from the fixture, with lookups by qualified name.
struct Fixture {
    files: Vec<ParsedFile>,
}

impl Fixture {
    fn parse() -> Fixture {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/polyglot");
        let scanned = lge_index::scan(&root, 1).unwrap();
        let listed: Vec<(&str, &str, &str)> = scanned
            .iter()
            .map(|f| (f.rel_path.as_str(), f.service.as_str(), f.lang))
            .collect();
        assert_eq!(
            listed,
            vec![
                ("notifier/src/audit.ts", "notifier", "typescript"),
                ("notifier/src/consumer.test.ts", "notifier", "typescript"),
                ("notifier/src/consumer.ts", "notifier", "typescript"),
                ("notifier/src/mailer.ts", "notifier", "typescript"),
                ("notifier/src/templates.ts", "notifier", "typescript"),
                ("orders_api/__init__.py", "orders_api", "python"),
                ("orders_api/events.py", "orders_api", "python"),
                ("orders_api/models.py", "orders_api", "python"),
                ("orders_api/repo.py", "orders_api", "python"),
                ("orders_api/routes.py", "orders_api", "python"),
                ("orders_api/tests/__init__.py", "orders_api", "python"),
                ("orders_api/tests/test_routes.py", "orders_api", "python"),
                ("web_gateway/src/ordersClient.ts", "web_gateway", "typescript"),
                ("web_gateway/src/routes.ts", "web_gateway", "typescript"),
                ("web_gateway/src/views.ts", "web_gateway", "typescript"),
            ],
            "package.json is not source"
        );
        let parsed = lge_index::parse_many(&scanned);
        assert_eq!(parsed.len(), scanned.len(), "every file has an extractor");
        let files = parsed
            .into_iter()
            .zip(&scanned)
            .map(|((path, result), scanned)| {
                let file = result.unwrap_or_else(|e| panic!("{path}: {e:#}"));
                let record = file.file.as_ref().unwrap();
                assert_eq!(
                    (record.path.as_str(), record.lang.as_str()),
                    (path.as_str(), scanned.lang)
                );
                assert_eq!(record.service, scanned.service);
                file
            })
            .collect();
        Fixture { files }
    }

    fn file(&self, path: &str) -> &ParsedFile {
        self.files
            .iter()
            .find(|f| f.file.as_ref().is_some_and(|r| r.path == path))
            .unwrap_or_else(|| panic!("no file {path}"))
    }

    fn name_of(&self, symbol_id: &str) -> &str {
        self.files
            .iter()
            .flat_map(|f| &f.symbols)
            .find(|s| s.id == symbol_id)
            .map(|s| s.qualified_name.as_str())
            .unwrap_or_else(|| panic!("no symbol with id {symbol_id}"))
    }

    /// (qualified name, kind) of the symbols of one file, the module first.
    fn symbols(&self, path: &str) -> Vec<(&str, &str)> {
        self.file(path)
            .symbols
            .iter()
            .map(|s| (s.qualified_name.as_str(), s.kind.as_str()))
            .collect()
    }

    /// (receiver, name) of the calls one symbol makes, in source order.
    fn calls(&self, path: &str, caller: &str) -> Vec<(&str, &str)> {
        self.file(path)
            .calls
            .iter()
            .filter(|c| self.name_of(&c.caller_id) == caller)
            .map(|c| (c.receiver.as_str(), c.name.as_str()))
            .collect()
    }

    /// (module, name, alias) of the imports of one file.
    fn imports(&self, path: &str) -> Vec<(&str, &str, &str)> {
        self.file(path)
            .imports
            .iter()
            .map(|i| (i.module.as_str(), i.name.as_str(), i.alias.as_str()))
            .collect()
    }

    /// (owner, key, detail, literal) of every fact of one kind, in file and line order.
    fn facts(&self, kind: &str) -> Vec<(&str, &str, &str, bool)> {
        self.files
            .iter()
            .flat_map(|f| &f.facts)
            .filter(|f| f.kind == kind)
            .map(|f| (self.name_of(&f.symbol_id), f.key.as_str(), f.detail.as_str(), f.literal))
            .collect()
    }
}

#[test]
fn polyglot_fixture_extracts_every_service() {
    let fx = Fixture::parse();

    // --- Symbols -----------------------------------------------------------
    assert_eq!(
        fx.symbols("web_gateway/src/routes.ts"),
        vec![
            ("web_gateway.src.routes", kind::MODULE),
            ("web_gateway.src.routes.getOrderHandler", kind::FUNCTION),
            ("web_gateway.src.routes.createOrderHandler", kind::FUNCTION),
        ]
    );
    assert_eq!(
        fx.symbols("web_gateway/src/ordersClient.ts"),
        vec![
            ("web_gateway.src.ordersClient", kind::MODULE),
            ("web_gateway.src.ordersClient.NewOrder", kind::INTERFACE),
            ("web_gateway.src.ordersClient.createOrder", kind::FUNCTION),
            ("web_gateway.src.ordersClient.fetchOrder", kind::FUNCTION),
        ]
    );
    assert_eq!(
        fx.symbols("web_gateway/src/views.ts"),
        vec![
            ("web_gateway.src.views", kind::MODULE),
            ("web_gateway.src.views.render", kind::FUNCTION),
        ]
    );
    assert_eq!(
        fx.symbols("notifier/src/consumer.ts"),
        vec![
            ("notifier.src.consumer", kind::MODULE),
            ("notifier.src.consumer.handleOrderCreated", kind::FUNCTION),
            ("notifier.src.consumer.start", kind::FUNCTION),
        ]
    );
    assert_eq!(
        fx.symbols("notifier/src/mailer.ts"),
        vec![
            ("notifier.src.mailer", kind::MODULE),
            ("notifier.src.mailer.Mailer", kind::INTERFACE),
            ("notifier.src.mailer.Mailer.send", kind::METHOD),
            ("notifier.src.mailer.BaseMailer", kind::CLASS),
            ("notifier.src.mailer.BaseMailer.envelope", kind::METHOD),
            ("notifier.src.mailer.SmtpMailer", kind::CLASS),
            ("notifier.src.mailer.SmtpMailer.send", kind::METHOD),
            ("notifier.src.mailer.sendEmail", kind::FUNCTION),
        ]
    );
    assert_eq!(
        fx.symbols("notifier/src/audit.ts"),
        vec![
            ("notifier.src.audit", kind::MODULE),
            ("notifier.src.audit.Delivery", kind::INTERFACE),
            ("notifier.src.audit.AuditLog", kind::CLASS),
            ("notifier.src.audit.AuditLog.recordDelivery", kind::METHOD),
        ]
    );
    assert_eq!(
        fx.symbols("notifier/src/templates.ts"),
        vec![
            ("notifier.src.templates", kind::MODULE),
            ("notifier.src.templates.render", kind::FUNCTION),
        ]
    );
    let test_case = "notifier.src.consumer.test.handleOrderCreated.records_the_delivery_of_the_email";
    assert_eq!(
        fx.symbols("notifier/src/consumer.test.ts"),
        vec![
            ("notifier.src.consumer.test", kind::MODULE),
            (test_case, kind::FUNCTION)
        ]
    );
    assert!(fx
        .symbols("orders_api/routes.py")
        .contains(&("orders_api.routes.create_order", kind::FUNCTION)));

    // The two names the resolver has to tell apart by name alone: `recordDelivery`
    // is defined once in the repository, `render` twice.
    let named = |name: &str| -> Vec<&str> {
        fx.files
            .iter()
            .flat_map(|f| &f.symbols)
            .filter(|s| s.name == name)
            .map(|s| s.qualified_name.as_str())
            .collect()
    };
    assert_eq!(
        named("recordDelivery"),
        vec!["notifier.src.audit.AuditLog.recordDelivery"]
    );
    assert_eq!(
        named("render"),
        vec!["notifier.src.templates.render", "web_gateway.src.views.render"]
    );

    let handler = fx
        .file("notifier/src/consumer.ts")
        .symbols
        .iter()
        .find(|s| s.name == "handleOrderCreated")
        .unwrap();
    assert_eq!(handler.service, "notifier");
    assert_eq!(handler.doc, "Email the customer of a new order.");
    assert_eq!(
        handler.signature,
        "async function handleOrderCreated(message, audit = auditLog, tpl = templates)"
    );
    assert_eq!((handler.start_line, handler.end_line), (16, 23));
    assert!(handler.body.starts_with("export async function handleOrderCreated("));
    assert!(handler
        .body
        .ends_with("audit.recordDelivery(order.order_id, email);\n}"));
    assert!(!handler.is_test);
    let mailer = &fx.file("notifier/src/mailer.ts").symbols[5];
    assert_eq!(
        mailer.signature,
        "class SmtpMailer extends BaseMailer implements Mailer"
    );
    assert_eq!(mailer.doc, "Delivers mail through the SMTP relay.");
    assert_eq!(
        fx.file("web_gateway/src/routes.ts").symbols[0].doc,
        "HTTP routes of the public gateway."
    );

    // Tests: the file name marks the module, the `it` block is a function symbol.
    for file in &fx.files {
        let path = &file.file.as_ref().unwrap().path;
        let in_test_file = path.ends_with("consumer.test.ts") || path.contains("/tests/");
        for s in &file.symbols {
            assert_eq!(s.is_test, in_test_file, "{}", s.qualified_name);
        }
    }

    // --- Call sites --------------------------------------------------------
    assert_eq!(
        fx.calls("notifier/src/consumer.ts", "notifier.src.consumer.handleOrderCreated"),
        vec![
            ("JSON", "parse"),
            ("message.content", "toString"),
            ("pool", "query"),
            // An untyped parameter: only the name `render` is known, and two functions have it.
            ("tpl", "render"),
            ("", "String"),
            // Imported from `./mailer`.
            ("", "sendEmail"),
            // An untyped parameter again, but only one `recordDelivery` exists.
            ("audit", "recordDelivery"),
        ]
    );
    assert_eq!(
        fx.calls("notifier/src/consumer.ts", "notifier.src.consumer.start"),
        vec![("channel", "assertQueue"), ("channel", "consume")]
    );
    assert_eq!(
        fx.calls("notifier/src/consumer.ts", "notifier.src.consumer"),
        vec![("", "Pool"), ("", "AuditLog")]
    );
    assert_eq!(
        fx.calls("notifier/src/mailer.ts", "notifier.src.mailer.sendEmail"),
        vec![("SmtpMailer()", "send"), ("", "SmtpMailer")]
    );
    assert_eq!(
        fx.calls("notifier/src/mailer.ts", "notifier.src.mailer.SmtpMailer.send"),
        vec![("this.transport", "sendMail"), ("self", "envelope")]
    );
    assert_eq!(
        fx.calls("notifier/src/consumer.test.ts", test_case),
        vec![
            ("jest", "fn"),
            ("jest", "fn"),
            ("Buffer", "from"),
            ("JSON", "stringify"),
            ("", "handleOrderCreated"),
            ("expect()", "toHaveBeenCalled"),
            ("", "expect"),
        ]
    );
    assert_eq!(
        fx.calls("web_gateway/src/routes.ts", "web_gateway.src.routes.getOrderHandler"),
        vec![
            ("", "fetchOrder"),
            ("res.type()", "end"),
            ("res", "type"),
            ("", "render")
        ]
    );
    assert_eq!(
        fx.calls("web_gateway/src/routes.ts", "web_gateway.src.routes.createOrderHandler"),
        vec![("", "createOrder"), ("res.status()", "json"), ("res", "status")]
    );
    assert_eq!(
        fx.calls(
            "web_gateway/src/ordersClient.ts",
            "web_gateway.src.ordersClient.fetchOrder"
        ),
        vec![("axios", "get")]
    );
    assert_eq!(
        fx.calls("orders_api/routes.py", "orders_api.routes.create_order"),
        vec![("", "insert_order"), ("", "publish_order_created")]
    );

    // --- Import sites ------------------------------------------------------
    assert_eq!(
        fx.imports("web_gateway/src/routes.ts"),
        vec![
            ("express", "Request", "Request"),
            ("express", "Response", "Response"),
            ("express", "Router", "Router"),
            ("./ordersClient", "createOrder", "createOrder"),
            ("./ordersClient", "fetchOrder", "fetchOrder"),
            ("./views", "render", "render"),
        ]
    );
    assert_eq!(
        fx.imports("web_gateway/src/ordersClient.ts"),
        vec![("axios", "default", "axios")]
    );
    assert_eq!(fx.imports("web_gateway/src/views.ts"), vec![]);
    assert_eq!(
        fx.imports("notifier/src/consumer.ts"),
        vec![
            ("amqplib", "Channel", "Channel"),
            ("pg", "Pool", "Pool"),
            ("./audit", "AuditLog", "AuditLog"),
            ("./mailer", "sendEmail", "sendEmail"),
            ("./templates", "*", "templates"),
        ]
    );
    assert_eq!(
        fx.imports("notifier/src/consumer.test.ts"),
        vec![("./consumer", "handleOrderCreated", "handleOrderCreated")]
    );
    assert_eq!(
        fx.imports("notifier/src/mailer.ts"),
        vec![("nodemailer", "default", "nodemailer")]
    );
    assert_eq!(fx.imports("notifier/src/audit.ts"), vec![]);
    assert!(fx
        .imports("orders_api/routes.py")
        .contains(&(".repo", "insert_order", "insert_order")));

    // --- Routes and API calls ----------------------------------------------
    assert_eq!(
        fx.facts(fact::API_EXPOSE),
        vec![
            ("orders_api.routes.create_order", "/orders", "POST", true),
            ("orders_api.routes.get_order", "/orders/{order_id}", "GET", true),
            // The Express handlers are functions of the file, so the routes are theirs.
            ("web_gateway.src.routes.getOrderHandler", "/api/orders/:id", "GET", true),
            ("web_gateway.src.routes.createOrderHandler", "/api/orders", "POST", true),
        ]
    );
    assert_eq!(
        fx.facts(fact::API_CALL),
        vec![
            (
                "web_gateway.src.ordersClient.createOrder",
                "http://orders-api/orders",
                "POST",
                true
            ),
            // A template: the pieces with `${}` holes, and not a literal.
            (
                "web_gateway.src.ordersClient.fetchOrder",
                "${ORDERS_URL}/orders/${id}",
                "GET",
                false
            ),
        ]
    );
    // Both client calls land on a route the Python service exposes.
    assert_eq!(normalize_route("http://orders-api/orders"), normalize_route("/orders"));
    assert_eq!(
        normalize_route("${ORDERS_URL}/orders/${id}"),
        normalize_route("/orders/{order_id}")
    );
    let get_order = fx
        .file("web_gateway/src/routes.ts")
        .facts
        .iter()
        .find(|f| f.key == "/api/orders/:id")
        .unwrap();
    assert_eq!((get_order.line, get_order.end_line), (21, 21));
    assert_eq!(get_order.file_path, "web_gateway/src/routes.ts");

    // --- Queues and tables -------------------------------------------------
    assert_eq!(
        fx.facts(fact::QUEUE_PUBLISH),
        vec![(
            "orders_api.events.publish_order_created",
            "order.created",
            "queue",
            true
        )]
    );
    assert_eq!(
        fx.facts(fact::QUEUE_CONSUME),
        // `channel.consume("order.created", handleOrderCreated)` is written in
        // `start`; the named handler is the consumer.
        vec![(
            "notifier.src.consumer.handleOrderCreated",
            "order.created",
            "queue",
            true
        )]
    );
    assert_eq!(
        fx.facts(fact::DB_READ),
        vec![
            ("notifier.src.consumer.handleOrderCreated", "customers", "", true),
            ("orders_api.repo.find_order", "orders", "", true),
        ]
    );
    assert_eq!(
        fx.facts(fact::DB_WRITE),
        vec![("orders_api.repo.insert_order", "orders", "", true)]
    );
    assert_eq!(
        fx.facts(fact::TABLE_MODEL),
        vec![("orders_api.models.Customer", "customers", "", true)]
    );

    // --- Classes and references --------------------------------------------
    assert_eq!(
        fx.facts(fact::INHERITS),
        vec![("notifier.src.mailer.SmtpMailer", "BaseMailer", "", true)]
    );
    assert_eq!(
        fx.facts(fact::IMPLEMENTS),
        vec![("notifier.src.mailer.SmtpMailer", "Mailer", "", true)]
    );
    assert_eq!(
        fx.facts(fact::REFERENCE),
        vec![
            ("notifier.src.consumer.start", "handleOrderCreated", "", true),
            // Column types, imported from SQLAlchemy.
            ("orders_api.models.Customer", "Integer", "", true),
            ("orders_api.models.Customer", "String", "", true),
            ("orders_api.repo.find_customer", "Customer", "", true),
            ("web_gateway.src.routes", "getOrderHandler", "", true),
            ("web_gateway.src.routes", "createOrderHandler", "", true),
        ]
    );
    assert_eq!(fx.facts(fact::DEFAULT_EXPORT), vec![]);

    // --- Every record belongs to its file ----------------------------------
    for file in &fx.files {
        let path = &file.file.as_ref().unwrap().path;
        let owns = |id: &str| file.symbols.iter().any(|s| s.id == id);
        assert_eq!(file.symbols[0].kind, kind::MODULE, "{path}");
        for s in &file.symbols {
            assert_eq!(&s.file_path, path);
            assert_eq!(s.body.is_empty(), s.kind == kind::MODULE, "{}", s.qualified_name);
            assert!(s.start_line >= 1 && s.end_line >= s.start_line, "{}", s.qualified_name);
        }
        for c in &file.calls {
            assert!(owns(&c.caller_id), "{path}: {c:?}");
            assert_eq!(&c.file_path, path);
        }
        for f in &file.facts {
            assert!(owns(&f.symbol_id), "{path}: {f:?}");
            assert_eq!(&f.file_path, path);
            assert!(f.line >= 1 && f.end_line >= f.line && !f.key.is_empty(), "{f:?}");
        }
        for i in &file.imports {
            assert_eq!(&i.file_path, path);
        }
    }
}
