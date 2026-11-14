//! Whole repositories, written out record by record.

use crate::support::Repo;
use lge_core::{fact, kind};

/// `tests/fixtures/services` as the original indexer described it: every
/// symbol, call site and import site of the eight Python files.
pub fn original_fixture() -> Repo {
    let mut r = Repo::new();
    r.file("auth_svc/__init__.py", "auth_svc");
    r.file("auth_svc/tokens.py", "auth_svc");
    r.file("shared/util.py", "shared");
    r.file("work_svc/__init__.py", "work_svc");
    r.file("work_svc/controller.py", "work_svc");
    r.file("work_svc/repository.py", "work_svc");
    r.file("work_svc/service.py", "work_svc");
    r.test_file("work_svc/tests/test_service.py", "work_svc");

    let verify_token = r.function("auth_svc/tokens.py", "verify_token");
    r.function("auth_svc/tokens.py", "decode");
    r.function("auth_svc/tokens.py", "rotate_signing_key");
    r.call(&verify_token, "", "decode", 6);

    let slugify = r.function("shared/util.py", "slugify");
    r.call(&slugify, "text.lower()", "replace", 3);
    r.call(&slugify, "text", "lower", 3);

    r.class("work_svc/repository.py", "TaskRepository");
    r.method("work_svc/repository.py", "TaskRepository", "save_task");
    r.method("work_svc/repository.py", "TaskRepository", "find_all");

    r.import("work_svc/service.py", ".repository", "TaskRepository", "TaskRepository");
    r.import("work_svc/service.py", "shared", "util", "util");
    r.class("work_svc/service.py", "TaskService");
    let create_task = r.method("work_svc/service.py", "TaskService", "create_task");
    let list_tasks = r.method("work_svc/service.py", "TaskService", "list_tasks");
    let validate_title = r.method("work_svc/service.py", "TaskService", "validate_title");
    r.call(&create_task, "self", "validate_title", 9);
    r.call(&create_task, "util", "slugify", 10);
    r.call(&create_task, "TaskRepository()", "save_task", 11);
    r.call(&create_task, "", "TaskRepository", 11);
    r.call(&list_tasks, "TaskRepository()", "find_all", 14);
    r.call(&list_tasks, "", "TaskRepository", 14);
    r.call(&validate_title, "", "ValueError", 18);

    r.import(
        "work_svc/controller.py",
        "work_svc.service",
        "TaskService",
        "TaskService",
    );
    r.import(
        "work_svc/controller.py",
        "auth_svc.tokens",
        "verify_token",
        "verify_token",
    );
    let create_handler = r.function("work_svc/controller.py", "create_task_handler");
    let list_handler = r.function("work_svc/controller.py", "list_tasks_handler");
    r.call(&create_handler, "", "verify_token", 8);
    r.call(&create_handler, "TaskService()", "create_task", 9);
    r.call(&create_handler, "", "TaskService", 9);
    r.call(&list_handler, "TaskService()", "list_tasks", 13);
    r.call(&list_handler, "", "TaskService", 13);

    r.import(
        "work_svc/tests/test_service.py",
        "work_svc.service",
        "TaskService",
        "TaskService",
    );
    let test = r.function("work_svc/tests/test_service.py", "test_create_task");
    r.call(&test, "TaskService()", "create_task", 5);
    r.call(&test, "", "TaskService", 5);
    r
}

/// The three services of `tests/fixtures/polyglot` as `docs/ARCHITECTURE.md`
/// lays them out: a Python API, a TypeScript gateway that calls it over HTTP,
/// and a TypeScript consumer of its queue.
pub fn polyglot() -> Repo {
    let mut r = Repo::new();

    // orders_api: Python, FastAPI style.
    for name in ["__init__", "routes", "repo", "events", "models"] {
        r.file(&format!("orders_api/{name}.py"), "orders_api");
    }
    r.test_file("orders_api/tests/test_routes.py", "orders_api");
    r.import("orders_api/routes.py", "fastapi", "APIRouter", "APIRouter");
    r.import("orders_api/routes.py", ".", "repo", "repo");
    r.import(
        "orders_api/routes.py",
        ".events",
        "publish_order_created",
        "publish_order_created",
    );
    let create_order = r.function("orders_api/routes.py", "create_order");
    let get_order = r.function("orders_api/routes.py", "get_order");
    r.fact(&create_order, fact::API_EXPOSE, "/orders", "POST", 9, true);
    r.fact(&get_order, fact::API_EXPOSE, "/orders/{order_id}", "GET", 16, true);
    r.call(&create_order, "repo", "insert_order", 11);
    r.call(&create_order, "", "publish_order_created", 12);
    r.call(&get_order, "repo", "find_order", 18);
    let insert_order = r.function("orders_api/repo.py", "insert_order");
    let find_order = r.function("orders_api/repo.py", "find_order");
    r.fact(&insert_order, fact::DB_WRITE, "orders", "", 6, true);
    r.fact(&find_order, fact::DB_READ, "orders", "", 11, true);
    r.call(&insert_order, "cursor", "execute", 6);
    let publish = r.function("orders_api/events.py", "publish_order_created");
    r.fact(&publish, fact::QUEUE_PUBLISH, "order.created", "queue", 8, true);
    let customer = r.class("orders_api/models.py", "Customer");
    r.fact(&customer, fact::TABLE_MODEL, "customers", "", 6, true);
    r.fact(&customer, fact::INHERITS, "Base", "", 5, true);
    r.import(
        "orders_api/tests/test_routes.py",
        "orders_api.routes",
        "create_order",
        "create_order",
    );
    let test_create_order = r.function("orders_api/tests/test_routes.py", "test_create_order");
    r.call(&test_create_order, "", "create_order", 5);

    // web_gateway: TypeScript, Express style.
    r.file("web_gateway/src/routes.ts", "web_gateway");
    r.file("web_gateway/src/ordersClient.ts", "web_gateway");
    r.file("web_gateway/src/views.ts", "web_gateway");
    r.import("web_gateway/src/routes.ts", "express", "default", "express");
    r.import(
        "web_gateway/src/routes.ts",
        "./ordersClient",
        "fetchOrder",
        "fetchOrder",
    );
    r.import(
        "web_gateway/src/routes.ts",
        "./ordersClient",
        "createOrder",
        "createOrder",
    );
    r.import("web_gateway/src/routes.ts", "./views", "render", "render");
    let get_handler = r.function("web_gateway/src/routes.ts", "getOrderHandler");
    let create_handler = r.function("web_gateway/src/routes.ts", "createOrderHandler");
    r.fact(&get_handler, fact::API_EXPOSE, "/api/orders/:id", "GET", 21, true);
    r.fact(&create_handler, fact::API_EXPOSE, "/api/orders", "POST", 22, true);
    r.call(&get_handler, "", "fetchOrder", 8);
    r.call(&get_handler, "", "render", 9);
    r.call(&get_handler, "res", "json", 10);
    r.call(&create_handler, "", "createOrder", 14);
    r.import("web_gateway/src/ordersClient.ts", "axios", "default", "axios");
    let create_client = r.function("web_gateway/src/ordersClient.ts", "createOrder");
    let fetch_client = r.function("web_gateway/src/ordersClient.ts", "fetchOrder");
    r.fact(
        &create_client,
        fact::API_CALL,
        "http://orders-api/orders",
        "POST",
        6,
        true,
    );
    r.fact(
        &fetch_client,
        fact::API_CALL,
        "${ORDERS_URL}/orders/${id}",
        "GET",
        10,
        false,
    );
    r.call(&create_client, "axios", "post", 6);
    r.call(&fetch_client, "axios", "get", 10);
    r.function("web_gateway/src/views.ts", "render");

    // notifier: TypeScript, amqplib style.
    for name in ["consumer", "mailer", "base", "audit", "templates"] {
        r.file(&format!("notifier/src/{name}.ts"), "notifier");
    }
    r.test_file("notifier/src/consumer.test.ts", "notifier");
    r.import("notifier/src/consumer.ts", "amqplib", "*", "amqp");
    r.import("notifier/src/consumer.ts", "./mailer", "sendEmail", "sendEmail");
    let handle = r.function("notifier/src/consumer.ts", "handleOrderCreated");
    let start = r.function("notifier/src/consumer.ts", "start");
    r.fact(&handle, fact::QUEUE_CONSUME, "order.created", "queue", 24, true);
    r.fact(&start, fact::REFERENCE, "handleOrderCreated", "", 24, true);
    r.fact(&handle, fact::DB_READ, "customers", "", 11, true);
    r.call(&handle, "", "sendEmail", 12);
    r.call(&handle, "audit", "recordDelivery", 13);
    r.call(&handle, "tpl", "render", 14);
    r.import("notifier/src/mailer.ts", "./base", "BaseMailer", "BaseMailer");
    r.function("notifier/src/mailer.ts", "sendEmail");
    let smtp = r.class("notifier/src/mailer.ts", "SmtpMailer");
    r.def("notifier/src/mailer.ts", "Mailer", kind::INTERFACE);
    r.fact(&smtp, fact::INHERITS, "BaseMailer", "", 9, true);
    r.fact(&smtp, fact::IMPLEMENTS, "Mailer", "", 9, true);
    r.class("notifier/src/base.ts", "BaseMailer");
    r.class("notifier/src/audit.ts", "AuditLog");
    r.method("notifier/src/audit.ts", "AuditLog", "recordDelivery");
    r.function("notifier/src/templates.ts", "render");
    r.import(
        "notifier/src/consumer.test.ts",
        "./consumer",
        "handleOrderCreated",
        "handleOrderCreated",
    );
    let test_handle = r.function("notifier/src/consumer.test.ts", "handles an order");
    r.call(&test_handle, "", "handleOrderCreated", 7);
    r
}

/// Input no indexer should produce: records repeated or pointing nowhere,
/// names bound twice, facts without a key. The resolver must stay
/// deterministic and honest on it.
pub fn untidy() -> Repo {
    let mut r = Repo::new();
    r.file("svc/app.py", "svc");
    r.file("svc/fast.py", "svc");
    r.file("svc/slow.py", "svc");
    r.file("lib/app.ts", "");
    r.file("lib/x.ts", "");
    r.file("lib/y.ts", "");
    r.file("lib/index.ts", "");
    let main = r.function("svc/app.py", "main");
    r.function("svc/fast.py", "parse");
    r.function("svc/slow.py", "parse");
    r.import("svc/app.py", ".fast", "parse", "parse");
    r.import("svc/app.py", ".slow", "parse", "parse");
    r.import("svc/app.py", "svc", "fast", "impl");
    r.import("svc/app.py", "svc", "slow", "impl");
    r.call(&main, "", "parse", 4);
    r.call(&main, "impl", "parse", 5);
    r.call(&main, "", "main", 6);
    r.call("no-such-symbol", "", "parse", 7);
    r.fact("no-such-symbol", fact::DB_READ, "orders", "", 1, true);
    r.fact(&main, fact::DB_READ, "  ", "", 2, true);
    r.fact(&main, fact::DB_READ, "Orders", "", 3, true);
    r.fact(&main, fact::DB_WRITE, "orders", "", 4, false);
    r.fact(&main, "no_such_kind", "x", "", 5, true);
    r.fact(&main, fact::API_CALL, "${url}", "", 6, false);
    r.fact(&main, fact::API_EXPOSE, "", "", 7, false);
    r.fact(&main, fact::INHERITS, "<T>", "", 8, true);

    r.import("lib/index.ts", "./x", "*", "*");
    r.import("lib/index.ts", "./y", "*", "");
    r.function("lib/x.ts", "dup");
    r.function("lib/y.ts", "dup");
    let go = r.function("lib/app.ts", "go");
    r.import("lib/app.ts", ".", "dup", "dup");
    r.call(&go, "", "dup", 3);

    // The same records once more.
    let again = r.input.clone();
    r.input.files.extend(again.files);
    r.input.symbols.extend(again.symbols);
    r.input.imports.extend(again.imports);
    r
}

pub fn all() -> Vec<(&'static str, Repo)> {
    vec![
        ("original fixture", original_fixture()),
        ("polyglot", polyglot()),
        ("untidy", untidy()),
    ]
}
