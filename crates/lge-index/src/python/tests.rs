use super::*;
use crate::BODY_CAP;

fn parse(path: &str, src: &str) -> ParsedFile {
    parse_python(path, "svc", src.as_bytes(), "h").unwrap()
}

/// Facts of one kind as (owner qualified name, key, detail, literal), in line order.
fn facts_of(p: &ParsedFile, kind: &str) -> Vec<(String, String, String, bool)> {
    let mut found: Vec<&lge_core::FactSite> = p.facts.iter().filter(|f| f.kind == kind).collect();
    found.sort_by_key(|f| f.line);
    found
        .into_iter()
        .map(|f| {
            let owner = p
                .symbols
                .iter()
                .find(|s| s.id == f.symbol_id)
                .map(|s| s.qualified_name.clone())
                .unwrap_or_default();
            (owner, f.key.clone(), f.detail.clone(), f.literal)
        })
        .collect()
}

fn row(owner: &str, key: &str, detail: &str, literal: bool) -> (String, String, String, bool) {
    (owner.to_string(), key.to_string(), detail.to_string(), literal)
}

#[test]
fn extracts_symbols_calls_imports() {
    let src = br#"
"""Task service."""
from .repo import TaskRepository as Repo
import auth.tokens

class TaskService:
    """Business logic."""
    def create_task(self, title):
        auth.tokens.verify(title)
        self.validate(title)
        return Repo().save(title)

    def validate(self, title):
        return len(title) > 0
"#;
    let p = parse_python("work/service.py", "work", src, "h").unwrap();
    let names: Vec<_> = p
        .symbols
        .iter()
        .map(|s| (s.qualified_name.as_str(), s.kind.as_str()))
        .collect();
    assert!(names.contains(&("work.service", "module")));
    assert!(names.contains(&("work.service.TaskService", "class")));
    assert!(names.contains(&("work.service.TaskService.create_task", "method")));
    let calls: Vec<_> = p.calls.iter().map(|c| (c.name.as_str(), c.receiver.as_str())).collect();
    assert!(calls.contains(&("verify", "auth.tokens")));
    assert!(calls.contains(&("validate", "self")));
    assert!(calls.contains(&("Repo", "")));
    assert!(calls.contains(&("save", "Repo()")));
    let imps: Vec<_> = p
        .imports
        .iter()
        .map(|i| (i.module.as_str(), i.name.as_str(), i.alias.as_str()))
        .collect();
    assert!(imps.contains(&(".repo", "TaskRepository", "Repo")));
    assert!(imps.contains(&("auth.tokens", "", "auth.tokens")));
    assert_eq!(p.symbols[0].doc, "Task service.");
}

#[test]
fn symbol_bodies_hold_the_definition_text() {
    let long_line = format!("    return \"{}\"\n", "é".repeat(BODY_CAP));
    let src = format!(
        "\"\"\"Doc.\"\"\"\nimport functools\n\n@functools.cache\ndef decorated(x):\n    return x\n\n\nclass Box:\n    def get(self):\n        return 1\n\n\ndef long():\n{long_line}"
    );
    let p = parse("svc/mod.py", &src);
    let body = |qn: &str| {
        p.symbols
            .iter()
            .find(|s| s.qualified_name == qn)
            .map(|s| s.body.clone())
            .unwrap()
    };

    assert_eq!(body("svc.mod"), "", "a module has no body");
    // The body starts at the decorator, like the line range.
    assert_eq!(
        body("svc.mod.decorated"),
        "@functools.cache\ndef decorated(x):\n    return x"
    );
    assert_eq!(body("svc.mod.Box"), "class Box:\n    def get(self):\n        return 1");
    assert_eq!(body("svc.mod.Box.get"), "def get(self):\n        return 1");

    let cut = body("svc.mod.long");
    assert!(cut.starts_with("def long():\n    return \"é"));
    assert!(cut.len() <= BODY_CAP && cut.len() >= BODY_CAP - 1, "{}", cut.len());
    assert!(cut.ends_with('é'), "cut on a char boundary");
}

#[test]
fn flask_routes_are_exposed() {
    let src = r#"
from flask import Blueprint, Flask

app = Flask(__name__)
bp = Blueprint("items", __name__, url_prefix="/items")

@app.route("/health")
def health():
    return "ok"

@app.route("/orders/<int:order_id>", methods=["GET", "post"])
def order(order_id):
    pass

@bp.get("/<item_id>")
def get_item(item_id):
    pass

@bp.route("", methods=METHODS)
def list_items():
    pass
"#;
    let p = parse("svc/web.py", src);
    assert_eq!(
        facts_of(&p, fact::API_EXPOSE),
        vec![
            row("svc.web.health", "/health", "GET", true),
            row("svc.web.order", "/orders/<int:order_id>", "GET", true),
            row("svc.web.order", "/orders/<int:order_id>", "POST", true),
            row("svc.web.get_item", "/items/<item_id>", "GET", true),
            // The method list is a variable: any method.
            row("svc.web.list_items", "/items", "ANY", true),
        ]
    );
    assert_eq!(
        facts_of(&p, fact::API_CALL),
        vec![],
        "a route decorator is not a client call"
    );
}

#[test]
fn fastapi_routes_are_exposed_with_their_router_prefix() {
    let src = r#"
from unittest import mock
from fastapi import APIRouter, FastAPI

API = "/api"
app = FastAPI()
router = APIRouter(prefix="/orders")
admin = APIRouter(prefix=settings.ADMIN_PREFIX)
v2 = APIRouter(prefix=API)

@app.get("/health")
async def health():
    pass

@router.post("")
def create_order(payload):
    pass

@router.get("/{order_id}")
def get_order(order_id):
    pass

@router.delete(f"/{kind}/items")
def delete_items():
    pass

@admin.put("/flags")
def set_flag():
    pass

@v2.api_route("/orders", methods=["PATCH"])
def patch_order():
    pass

@mock.patch("orders.repo.find")
def test_patched(find):
    pass
"#;
    let p = parse("svc/api.py", src);
    assert_eq!(
        facts_of(&p, fact::API_EXPOSE),
        vec![
            row("svc.api.health", "/health", "GET", true),
            row("svc.api.create_order", "/orders", "POST", true),
            row("svc.api.get_order", "/orders/{order_id}", "GET", true),
            // An interpolated route keeps its hole and is not literal.
            row("svc.api.delete_items", "/orders/${kind}/items", "DELETE", false),
            // A prefix this file does not spell out stays a hole.
            row("svc.api.set_flag", "${settings.ADMIN_PREFIX}/flags", "PUT", false),
            // A prefix held in a constant is read through, and is not literal.
            row("svc.api.patch_order", "/api/orders", "PATCH", false),
        ]
    );
    let route = p.facts.iter().find(|f| f.key == "/orders/{order_id}").unwrap();
    assert_eq!((route.line, route.end_line), (19, 19), "the evidence is the decorator");
    assert_eq!(lge_core::normalize_route(&route.key), "/orders/{}");
}

#[test]
fn http_client_calls_are_api_calls() {
    let src = r#"
import httpx
import requests

BASE = "http://orders-api"
ORDERS_URL = "http://orders-api/orders"

def fetch(order_id, session, cache, url):
    requests.get("http://orders-api/orders")
    requests.post(ORDERS_URL, json={})
    httpx.get(f"{BASE}/orders/{order_id}")
    session.delete("/orders/" + str(order_id))
    session.request("put", f"/orders/{order_id}")
    cache.get("/orders/1")
    cache.get("user")
    requests.get(url)
    target = f"http://billing/invoices/{order_id}"
    requests.patch(target)
    self.http.request(verb, url="https://audit/log")
    conn.request("GET", "/status")
    conn.request(verb, "/status")

def with_client():
    with httpx.Client() as c:
        c.get("/health")
    s = requests.Session()
    s.post(url="/login")
"#;
    let p = parse("svc/client.py", src);
    assert_eq!(
        facts_of(&p, fact::API_CALL),
        vec![
            row("svc.client.fetch", "http://orders-api/orders", "GET", true),
            // Through a variable: the URL is known, but it is not a literal at the call.
            row("svc.client.fetch", "http://orders-api/orders", "POST", false),
            row("svc.client.fetch", "http://orders-api/orders/${order_id}", "GET", false),
            row("svc.client.fetch", "/orders/${str(order_id)}", "DELETE", false),
            row("svc.client.fetch", "/orders/${order_id}", "PUT", false),
            // cache.get(...) is not HTTP; requests.get(url) names no URL.
            row(
                "svc.client.fetch",
                "http://billing/invoices/${order_id}",
                "PATCH",
                false
            ),
            row("svc.client.fetch", "https://audit/log", "ANY", true),
            // `request` with a verb and a path is HTTP whatever the receiver is called.
            row("svc.client.fetch", "/status", "GET", true),
            row("svc.client.with_client", "/health", "GET", true),
            row("svc.client.with_client", "/login", "POST", true),
        ]
    );
    let templated = p.facts.iter().find(|f| f.key == "/orders/${str(order_id)}").unwrap();
    assert_eq!(lge_core::normalize_route(&templated.key), "/orders/{}");
}

#[test]
fn sql_in_strings_reads_and_writes_tables() {
    let src = r#"
ORDERS = "orders"
COUNT_SQL = "SELECT count(*) FROM orders"

def load(cur, order_id, table):
    """Select the order from the table."""
    cur.execute("SELECT id, total FROM orders WHERE id = %s", (order_id,))
    cur.execute(f"SELECT * FROM customers WHERE id = {order_id}")
    cur.execute(
        "INSERT INTO audit_log (order_id) "
        "VALUES (%s)",
        (order_id,),
    )
    cur.execute("UPDATE " + ORDERS + " SET seen = 1")
    cur.execute(f"DELETE FROM {table}")
    cur.execute(f"SELECT * FROM orders_{order_id}")
    cur.execute(COUNT_SQL)
    cur.execute("SELECT 1\nFROM refunds")
    log.info("could not load data from orders")
"#;
    let p = parse("svc/repo.py", src);
    assert_eq!(
        facts_of(&p, fact::DB_READ),
        vec![
            // The constant itself sits at module level.
            row("svc.repo", "orders", "", true),
            row("svc.repo.load", "orders", "", true),
            // An f-string that interpolates only a value still names its table literally.
            row("svc.repo.load", "customers", "", true),
            // SQL kept in a module constant, run here: known, not literal.
            row("svc.repo.load", "orders", "", false),
            row("svc.repo.load", "refunds", "", true),
        ]
    );
    assert_eq!(
        facts_of(&p, fact::DB_WRITE),
        vec![
            row("svc.repo.load", "audit_log", "", true),
            // The table name comes from a constant: not literal.
            row("svc.repo.load", "orders", "", false),
        ]
    );
    let insert = p.facts.iter().find(|f| f.key == "audit_log").unwrap();
    assert_eq!(
        (insert.line, insert.end_line),
        (10, 11),
        "the evidence is the whole string"
    );
}

#[test]
fn orm_models_name_their_table() {
    let src = r#"
TABLE = "billing.invoices"

class Customer(Base):
    __tablename__ = "customers"
    id = Column(Integer, primary_key=True)

class Order(models.Model):
    total = models.IntegerField()

    class Meta:
        db_table = "shop_orders"
        ordering = ["id"]

class Plain:
    db_table = "not_a_model"

class Invoice(Base):
    __tablename__ = TABLE

def helper():
    __tablename__ = "local_variable"
"#;
    let p = parse("svc/models.py", src);
    assert_eq!(
        facts_of(&p, fact::TABLE_MODEL),
        vec![
            row("svc.models.Customer", "customers", "", true),
            // Django: the fact is on the model, not on its Meta class.
            row("svc.models.Order", "shop_orders", "", true),
            row("svc.models.Invoice", "invoices", "", false),
        ]
    );
}

#[test]
fn pika_calls_publish_and_consume() {
    let src = r#"
import pika

QUEUE = "order.created"

class Events:
    def publish(self, channel, body, queue_name):
        channel.basic_publish(exchange="", routing_key="order.created", body=body)
        channel.basic_publish("orders", "order.paid", body)
        channel.basic_publish(exchange="", routing_key=QUEUE, body=body)
        channel.basic_publish(exchange="", routing_key=queue_name, body=body)
        channel.basic_publish(exchange="", routing_key=f"order.{body.kind}", body=body)

    def consume(self, channel):
        channel.basic_consume(queue="order.created", on_message_callback=self.on_message)
        channel.basic_consume(self.on_message, QUEUE)

    def on_message(self, ch, method, properties, body):
        pass
"#;
    let p = parse("svc/events.py", src);
    assert_eq!(
        facts_of(&p, fact::QUEUE_PUBLISH),
        vec![
            // Default exchange: the routing key is the queue.
            row("svc.events.Events.publish", "order.created", "queue", true),
            // Named exchange: the exchange is the target.
            row("svc.events.Events.publish", "orders", "exchange", true),
            row("svc.events.Events.publish", "order.created", "queue", false),
            // A queue name passed in as a parameter is unknown: no fact.
            row("svc.events.Events.publish", "order.${body.kind}", "queue", false),
        ]
    );
    assert_eq!(
        facts_of(&p, fact::QUEUE_CONSUME),
        vec![
            row("svc.events.Events.consume", "order.created", "queue", true),
            // pika before 1.0: callback first, queue second.
            row("svc.events.Events.consume", "order.created", "queue", false),
        ]
    );
}

#[test]
fn celery_tasks_consume_and_send_task_publishes() {
    let src = r#"
from celery import Celery, shared_task

app = Celery("orders")

@app.task(name="orders.process")
def process(order_id):
    pass

@shared_task(name="orders.cleanup")
def cleanup():
    pass

@app.task
def unnamed():
    pass

def enqueue(order_id):
    app.send_task("orders.process", args=[order_id])
"#;
    let p = parse("svc/tasks.py", src);
    assert_eq!(
        facts_of(&p, fact::QUEUE_CONSUME),
        vec![
            row("svc.tasks.process", "orders.process", "topic", true),
            row("svc.tasks.cleanup", "orders.cleanup", "topic", true)
        ]
    );
    assert_eq!(
        facts_of(&p, fact::QUEUE_PUBLISH),
        vec![row("svc.tasks.enqueue", "orders.process", "topic", true)]
    );
}

#[test]
fn class_bases_are_inherits_facts() {
    let src = r#"
from typing import Generic
from . import mixins
from .base import Model

Base = declarative_base()

class A(Model, mixins.Audit, Generic[T], object, Base, metaclass=Meta):
    pass

class B(A):
    pass

class C:
    pass

class D(make_base()):
    pass
"#;
    let p = parse("svc/classes.py", src);
    assert_eq!(
        facts_of(&p, fact::INHERITS),
        vec![
            row("svc.classes.A", "Model", "", true),
            row("svc.classes.A", "mixins.Audit", "", true),
            row("svc.classes.A", "Generic", "", true),
            // `object` and the variable `Base` are skipped; `metaclass=` is not a base.
            row("svc.classes.B", "A", "", true),
        ]
    );
}

#[test]
fn names_passed_around_are_references() {
    let src = r#"
from .handlers import on_created
from . import handlers

DEFAULT = 3

def helper(x):
    return x

@login_required
@decorators.audited
@staticmethod
def view(request, callback):
    local = make()
    register(on_created, handlers.on_deleted, key=helper)
    register(request, callback, local, DEFAULT, len, request.user)
    for item in items():
        register(item)
    run(lambda evt: register(evt))
    session.query(Order)
    with open(request.path) as fh:
        register(fh)

class Worker:
    def start(self, bus):
        bus.subscribe(self.handle, self.name)
        bus.subscribe(cls.handle)

    def handle(self, event):
        return [register(e) for e in event]

    @property
    def title(self):
        return register(Worker)

    @title.setter
    def title(self, value):
        pass
"#;
    let p = parse("svc/views.py", src);
    assert_eq!(
        facts_of(&p, fact::REFERENCE),
        vec![
            // A decorator used without a call, on the decorated symbol. `@staticmethod` is a builtin.
            row("svc.views.view", "login_required", "", true),
            row("svc.views.view", "decorators.audited", "", true),
            row("svc.views.view", "on_created", "", true),
            row("svc.views.view", "handlers.on_deleted", "", true),
            row("svc.views.view", "helper", "", true),
            // Parameters, locals, loop and lambda variables, module variables and builtins are not symbols.
            row("svc.views.view", "Order", "", true),
            // `self.handle` is a method of the class; `self.name` is data.
            row("svc.views.Worker.start", "self.handle", "", true),
            row("svc.views.Worker.start", "self.handle", "", true),
            row("svc.views.Worker.title", "Worker", "", true),
        ]
    );
    // A property and its setter share an id: the symbol is kept once.
    assert_eq!(
        p.symbols
            .iter()
            .filter(|s| s.qualified_name == "svc.views.Worker.title")
            .count(),
        1
    );
}

#[test]
fn decorator_arguments_are_about_the_decorated_symbol() {
    let src = r#"
from .auth import check_token
from .errors import on_error

@router.post("/orders", dependencies=[Depends(check_token)])
@retry(handler=on_error)
def create_order():
    pass
"#;
    let p = parse("svc/api.py", src);
    assert_eq!(
        facts_of(&p, fact::REFERENCE),
        vec![
            row("svc.api.create_order", "check_token", "", true),
            row("svc.api.create_order", "on_error", "", true)
        ]
    );
    assert_eq!(
        facts_of(&p, fact::API_EXPOSE),
        vec![row("svc.api.create_order", "/orders", "POST", true)]
    );
    // The decorator calls themselves are still made by the enclosing scope.
    let module_id = symbol_id("svc/api.py", "svc.api", kind::MODULE);
    let callers: Vec<(&str, bool)> = p
        .calls
        .iter()
        .map(|c| (c.name.as_str(), c.caller_id == module_id))
        .collect();
    assert_eq!(callers, vec![("post", true), ("Depends", true), ("retry", true)]);
}

#[test]
fn facts_belong_to_the_innermost_symbol() {
    let src = r#"
import requests

requests.get("http://a/top")

class Client:
    requests.get("http://a/class")

    def call(self):
        requests.get("http://a/method")

        def inner():
            requests.get("http://a/inner")

def fn():
    requests.get("http://a/fn")
"#;
    let p = parse("svc/mod.py", src);
    assert_eq!(
        facts_of(&p, fact::API_CALL),
        vec![
            row("svc.mod", "http://a/top", "GET", true),
            row("svc.mod.Client", "http://a/class", "GET", true),
            row("svc.mod.Client.call", "http://a/method", "GET", true),
            row("svc.mod.Client.call.inner", "http://a/inner", "GET", true),
            row("svc.mod.fn", "http://a/fn", "GET", true),
        ]
    );
    let top = &p.facts[0];
    let module_id = symbol_id("svc/mod.py", "svc.mod", kind::MODULE);
    assert_eq!(top.symbol_id, module_id);
    assert_eq!(top.file_path, "svc/mod.py");
    assert_eq!(
        top.id,
        stable_id(&[
            "fact",
            "svc/mod.py",
            &module_id,
            fact::API_CALL,
            "http://a/top",
            "GET",
            "4"
        ])
    );
    assert_eq!((top.line, top.end_line), (4, 4));
}

#[test]
fn ids_are_unique_within_a_file() {
    let src = r#"
try:
    import json
except ImportError:
    import json

def f(cur):
    cur.execute("SELECT * FROM t JOIN t ON 1 = 1"); register(g, g)

def f(cur):
    pass
"#;
    let p = parse("svc/dup.py", src);
    let unique = |ids: Vec<&str>| {
        let mut seen = HashSet::new();
        ids.iter().all(|id| seen.insert(*id))
    };
    assert!(unique(p.symbols.iter().map(|s| s.id.as_str()).collect()));
    assert!(unique(p.calls.iter().map(|c| c.id.as_str()).collect()));
    assert!(unique(p.imports.iter().map(|i| i.id.as_str()).collect()));
    assert!(unique(p.facts.iter().map(|f| f.id.as_str()).collect()));
    assert_eq!(p.imports.len(), 1);
    assert_eq!(facts_of(&p, fact::DB_READ), vec![row("svc.dup.f", "t", "", true)]);
    assert_eq!(facts_of(&p, fact::REFERENCE), vec![row("svc.dup.f", "g", "", true)]);
}

#[test]
fn deep_nesting_does_not_overflow_the_stack() {
    // 5,000 nested lists: far beyond anything hand-written.
    let src = format!(
        "x = {}1{}\n\ndef after():\n    pass\n",
        "[".repeat(5000),
        "]".repeat(5000)
    );
    let p = parse("svc/deep.py", &src);
    assert!(p.symbols.iter().any(|s| s.qualified_name == "svc.deep.after"));

    // 300 functions, each inside the one before: deeper than the limit follows.
    let mut nested = String::new();
    for i in 0..300 {
        nested.push_str(&format!("{}def f{i}():\n", " ".repeat(i)));
    }
    nested.push_str(&format!("{}pass\n", " ".repeat(300)));
    let p = parse("svc/nested.py", &nested);
    assert!(p.symbols.iter().any(|s| s.name == "f100"));

    let chain = format!(
        "q = {}\n",
        ["'SELECT a '", "'FROM orders '", "x"].repeat(2000).join(" + ")
    );
    let p = parse("svc/chain.py", &chain);
    assert_eq!(facts_of(&p, fact::DB_READ), vec![row("svc.chain", "orders", "", true)]);
}
