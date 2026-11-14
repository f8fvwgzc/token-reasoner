//! Storage and messaging facts: tables read and written (SQL text, Prisma),
//! ORM models (TypeORM, Sequelize), and queues published to and consumed from
//! (amqplib, NestJS microservices, BullMQ).

use super::strings::{dotted, text_of};
use super::{is_function_value, lines, object_prop, text, Ctx, Scope, Want};
use crate::facts as shared;
use lge_core::fact;
use tree_sitter::Node;

/// Prisma model operations that only read.
const PRISMA_READS: &[&str] = &[
    "findMany",
    "findFirst",
    "findFirstOrThrow",
    "findUnique",
    "findUniqueOrThrow",
    "count",
    "aggregate",
    "groupBy",
];

/// Prisma model operations that write.
const PRISMA_WRITES: &[&str] = &[
    "create",
    "createMany",
    "createManyAndReturn",
    "update",
    "updateMany",
    "upsert",
    "delete",
    "deleteMany",
];

/// Packages whose `Queue` and `Worker` classes are job queues.
const QUEUE_PACKAGES: &[&str] = &["bullmq", "bull"];

/// The package NestJS message clients come from.
const MESSAGE_PACKAGE: &str = "@nestjs/microservices";

/// NestJS client types whose `emit` and `send` publish a message.
const MESSAGE_CLIENT_TYPES: &[&str] = &[
    "ClientProxy",
    "ClientKafka",
    "ClientRMQ",
    "ClientNats",
    "ClientRedis",
    "ClientMqtt",
    "ClientTCP",
];

/// The last segment of a dotted name, lower-cased, without a leading `_` or `$`.
fn last_segment(name: &str) -> String {
    name.rsplit('.')
        .next()
        .unwrap_or(name)
        .trim_start_matches(['_', '$'])
        .to_ascii_lowercase()
}

impl Ctx<'_> {
    /// Remembers a variable whose declared type makes it a message client.
    pub(super) fn note_type(&mut self, name: &str, declared: Option<Node>) {
        let Some(declared) = declared else { return };
        let written = text(declared, self.src).trim_start_matches(':').trim();
        if MESSAGE_CLIENT_TYPES.contains(&written) {
            self.message_clients.insert(name.to_string());
        }
    }

    /// True when `class` (the callee of a `new`) is `wanted` imported from a queue package.
    fn is_queue_class(&self, class: Node, wanted: &str) -> bool {
        let from_queue_package = |module: &str| QUEUE_PACKAGES.contains(&module);
        match class.kind() {
            "identifier" => self
                .import_sources
                .get(text(class, self.src))
                .is_some_and(|(module, name)| name == wanted && from_queue_package(module)),
            // `new bullmq.Queue("q")`
            "member_expression" => {
                let object = class.child_by_field_name("object").map(|o| text(o, self.src));
                let property = class.child_by_field_name("property").map(|p| text(p, self.src));
                property == Some(wanted)
                    && object
                        .and_then(|o| self.import_sources.get(o))
                        .is_some_and(|(module, _)| from_queue_package(module))
            }
            _ => false,
        }
    }

    /// The queue a `new Queue("name")` expression opens.
    fn queue_of(&self, value: Node) -> Option<(String, bool)> {
        if value.kind() != "new_expression" {
            return None;
        }
        let class = value.child_by_field_name("constructor")?;
        if !self.is_queue_class(class, "Queue") {
            return None;
        }
        let name = super::arguments(value).first().copied()?;
        self.key_of(name, None)
    }

    fn is_channel(&self, object: Node) -> bool {
        let Some(name) = dotted(object, self.src) else {
            return false;
        };
        let last = last_segment(&name);
        self.channels.contains(&name) || last.contains("channel") || matches!(last.as_str(), "ch" | "chan")
    }

    /// True when `emit` and `send` on `object` publish a message: a variable
    /// declared as a NestJS client, or, in a file that imports the NestJS
    /// microservices package, one named like a client. Elsewhere `client.emit`
    /// is an event emitter.
    fn is_message_client(&self, object: Node) -> bool {
        let Some(name) = dotted(object, self.src) else {
            return false;
        };
        if self.message_clients.contains(&name) {
            return true;
        }
        let last = last_segment(&name);
        let named_like_one = last.ends_with("client") || last.ends_with("proxy");
        named_like_one
            && self
                .import_sources
                .values()
                .any(|(module, _)| module == MESSAGE_PACKAGE)
    }

    /// Notes what a variable is bound to when that gives it a role: a router,
    /// an HTTP client, an AMQP channel, a message client or a job queue.
    pub(super) fn note_binding(&mut self, name: &str, value: Node) {
        let value = super::unwrap(value);
        if let Some(queue) = self.queue_of(value) {
            self.queues.insert(name.to_string(), queue);
            return;
        }
        let maker = match value.kind() {
            "call_expression" => value.child_by_field_name("function"),
            "new_expression" => value.child_by_field_name("constructor"),
            _ => None,
        };
        let Some(maker) = maker.and_then(|m| dotted(m, self.src)) else {
            return;
        };
        let last = maker.rsplit('.').next().unwrap_or(&maker);
        match maker.as_str() {
            "express" | "fastify" | "Fastify" => {
                self.routers.insert(name.to_string());
            }
            "axios.create" | "got.extend" | "ky.create" | "ky.extend" => {
                let base = super::arguments(value)
                    .first()
                    .filter(|options| options.kind() == "object")
                    .and_then(|options| {
                        object_prop(*options, "baseURL", self.src)
                            .or_else(|| object_prop(*options, "prefixUrl", self.src))
                    })
                    .and_then(|base| self.key_of(base, None));
                self.http_clients.insert(name.to_string(), base);
            }
            "ClientProxyFactory.create" => {
                self.message_clients.insert(name.to_string());
            }
            _ => match last {
                "Router" | "Hono" => {
                    self.routers.insert(name.to_string());
                }
                "createChannel" | "createConfirmChannel" => {
                    self.channels.insert(name.to_string());
                }
                _ => {}
            },
        }
    }

    /// Table facts for SQL text. `direct` is false when the text came through a variable.
    pub(super) fn sql_pieces(&mut self, pieces: &[shared::Piece], at: (i64, i64), scope: &Scope, direct: bool) {
        for r in shared::sql_refs(pieces) {
            let (table, literal) = match r.hole {
                None => (r.table, direct),
                // The table name is interpolated: known only when it is a constant of this file.
                Some(i) => {
                    let Some(shared::Piece::Hole(expr)) = pieces.get(i) else {
                        continue;
                    };
                    let Some(table) = self.const_text(expr, scope.locals).and_then(|t| shared::table_name(&t)) else {
                        continue;
                    };
                    (table, false)
                }
            };
            let kind = if r.write { fact::DB_WRITE } else { fact::DB_READ };
            self.fact(scope.owner_id, kind, &table, "", at, literal);
        }
    }
}

/// SQL in a string, a template string or a concatenation.
pub(super) fn sql_facts(node: Node, ctx: &mut Ctx, scope: &Scope) {
    if ctx.sql_seen.contains(&node.id()) {
        return;
    }
    let Some(t) = text_of(node, ctx.src) else { return };
    ctx.sql_seen.extend(t.nodes.iter().copied());
    ctx.sql_pieces(&t.pieces, lines(node), scope, true);
}

/// Storage and messaging facts from one call.
pub(super) fn call_facts(call: Node, args: &[Node], ctx: &mut Ctx, scope: &Scope) {
    let Some(f) = call
        .child_by_field_name("function")
        .filter(|f| f.kind() == "member_expression")
    else {
        return;
    };
    let (Some(object), Some(property)) = (f.child_by_field_name("object"), f.child_by_field_name("property")) else {
        return;
    };
    match text(property, ctx.src) {
        "sendToQueue" => {
            if let Some((queue, literal)) = args.first().and_then(|n| ctx.key_of(*n, scope.locals)) {
                ctx.fact(
                    scope.owner_id,
                    fact::QUEUE_PUBLISH,
                    &queue,
                    "queue",
                    lines(call),
                    literal,
                );
            }
        }
        "publish" => amqp_publish(call, object, args, ctx, scope),
        "consume" => amqp_consume(call, object, args, ctx, scope),
        "emit" | "send" if ctx.is_message_client(object) => {
            if let Some((pattern, literal)) = args.first().and_then(|n| ctx.key_of(*n, scope.locals)) {
                ctx.fact(
                    scope.owner_id,
                    fact::QUEUE_PUBLISH,
                    &pattern,
                    "topic",
                    lines(call),
                    literal,
                );
            }
        }
        "add" | "addBulk" => {
            let queue = ctx
                .queue_of(object)
                .or_else(|| dotted(object, ctx.src).and_then(|name| ctx.queues.get(&name).cloned()));
            if let Some((queue, literal)) = queue {
                ctx.fact(
                    scope.owner_id,
                    fact::QUEUE_PUBLISH,
                    &queue,
                    "queue",
                    lines(call),
                    literal,
                );
            }
        }
        "init" | "define" => sequelize_model(call, object, args, ctx, scope),
        operation => prisma(call, object, operation, ctx, scope),
    }
}

/// amqplib `channel.publish(exchange, routingKey, content)`. On the default
/// exchange (`""`) the routing key is the queue name; otherwise the exchange is the target.
fn amqp_publish(call: Node, object: Node, args: &[Node], ctx: &mut Ctx, scope: &Scope) {
    if args.len() < 3 || !ctx.is_channel(object) {
        return;
    }
    let Some((exchange, exchange_literal)) = ctx.key_of(args[0], scope.locals) else {
        return;
    };
    if !exchange.is_empty() {
        ctx.fact(
            scope.owner_id,
            fact::QUEUE_PUBLISH,
            &exchange,
            "exchange",
            lines(call),
            exchange_literal,
        );
        return;
    }
    if let Some((queue, queue_literal)) = ctx.key_of(args[1], scope.locals) {
        ctx.fact(
            scope.owner_id,
            fact::QUEUE_PUBLISH,
            &queue,
            "queue",
            lines(call),
            exchange_literal && queue_literal,
        );
    }
}

/// amqplib `channel.consume(queue, handler)`.
fn amqp_consume(call: Node, object: Node, args: &[Node], ctx: &mut Ctx, scope: &Scope) {
    let Some((queue, literal)) = args.first().and_then(|n| ctx.key_of(*n, scope.locals)) else {
        return;
    };
    let handler = args.get(1).copied();
    // On something not called a channel, only an inline callback says "consumer".
    if !ctx.is_channel(object) && !handler.is_some_and(is_function_value) {
        return;
    }
    let recorded = ctx.out.facts.len();
    ctx.fact(
        scope.owner_id,
        fact::QUEUE_CONSUME,
        &queue,
        "queue",
        lines(call),
        literal,
    );
    if let Some(handler) = handler {
        ctx.handled_by(handler, recorded, scope);
    }
}

/// Sequelize `Order.init(attributes, { tableName: "orders" })` and
/// `sequelize.define("Order", attributes, { tableName: "orders" })`.
fn sequelize_model(call: Node, object: Node, args: &[Node], ctx: &mut Ctx, scope: &Scope) {
    let table = args
        .iter()
        .filter(|a| a.kind() == "object")
        .find_map(|options| object_prop(*options, "tableName", ctx.src));
    let Some((name, literal)) = table.and_then(|t| ctx.key_of(t, scope.locals)) else {
        return;
    };
    let Some(table) = shared::table_name(&name) else {
        return;
    };
    // `super.init(...)` in a static method: the model is the class around it.
    let inside_class = matches!(object.kind(), "super" | "this");
    let owner = match scope.class {
        Some(class) if inside_class => class.id.as_str(),
        _ => scope.owner_id,
    };
    let index = ctx.out.facts.len();
    ctx.fact(owner, fact::TABLE_MODEL, &table, "", lines(call), literal);
    // `Order.init(...)` after `class Order extends Model {}`: the model is that class.
    if object.kind() == "identifier" && ctx.out.facts.len() > index {
        ctx.reown(index, text(object, ctx.src), scope, Want::Class);
    }
}

/// Prisma `prisma.order.findMany()`: the model name stands for its table.
fn prisma(call: Node, object: Node, operation: &str, ctx: &mut Ctx, scope: &Scope) {
    let write = PRISMA_WRITES.contains(&operation);
    if !write && !PRISMA_READS.contains(&operation) {
        return;
    }
    if object.kind() != "member_expression" {
        return;
    }
    let (Some(client), Some(model)) = (
        object.child_by_field_name("object"),
        object.child_by_field_name("property"),
    ) else {
        return;
    };
    let is_prisma = dotted(client, ctx.src).is_some_and(|name| last_segment(&name).contains("prisma"));
    let model = text(model, ctx.src);
    if !is_prisma || model.starts_with('$') {
        return;
    }
    let kind = if write { fact::DB_WRITE } else { fact::DB_READ };
    // The table name is derived from the property, not written out.
    ctx.fact(scope.owner_id, kind, model, "", lines(call), false);
}

/// BullMQ `new Worker("queue", processor)`: the processor consumes the queue.
pub(super) fn new_facts(new: Node, args: &[Node], ctx: &mut Ctx, scope: &Scope) {
    let is_worker = new
        .child_by_field_name("constructor")
        .is_some_and(|class| ctx.is_queue_class(class, "Worker"));
    if !is_worker {
        return;
    }
    let Some((queue, literal)) = args.first().and_then(|n| ctx.key_of(*n, scope.locals)) else {
        return;
    };
    let recorded = ctx.out.facts.len();
    ctx.fact(
        scope.owner_id,
        fact::QUEUE_CONSUME,
        &queue,
        "queue",
        lines(new),
        literal,
    );
    if let Some(handler) = args.get(1) {
        ctx.handled_by(*handler, recorded, scope);
    }
}

/// TypeORM `@Entity("orders")`, `@Entity({ name: "orders" })` and
/// sequelize-typescript `@Table({ tableName: "orders" })` on a class.
pub(super) fn class_decorator(call: Node, name: &str, args: &[Node], class_id: &str, ctx: &mut Ctx, scope: &Scope) {
    let option = match name {
        "Entity" => "name",
        "Table" => "tableName",
        _ => return,
    };
    let Some(first) = args.first().copied() else { return };
    let written = if first.kind() == "object" {
        object_prop(first, option, ctx.src)
    } else {
        // Only `@Entity` takes the table name as its first argument.
        (name == "Entity").then_some(first)
    };
    let Some((table, literal)) = written.and_then(|w| ctx.key_of(w, scope.locals)) else {
        return;
    };
    if let Some(table) = shared::table_name(&table) {
        ctx.fact(class_id, fact::TABLE_MODEL, &table, "", lines(call), literal);
    }
}

/// NestJS `@EventPattern("order.created")` and `@MessagePattern("orders.get")`
/// on a method: the method consumes that topic.
pub(super) fn pattern_decorator(call: Node, name: &str, args: &[Node], def_id: &str, ctx: &mut Ctx, scope: &Scope) {
    if !matches!(name, "EventPattern" | "MessagePattern") {
        return;
    }
    if let Some((pattern, literal)) = args.first().and_then(|n| ctx.key_of(*n, scope.locals)) {
        ctx.fact(def_id, fact::QUEUE_CONSUME, &pattern, "topic", lines(call), literal);
    }
}
