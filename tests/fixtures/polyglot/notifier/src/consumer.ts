// Consumes order events and emails the customer.
import type { Channel } from "amqplib";
import { Pool } from "pg";

import { AuditLog } from "./audit";
import { sendEmail } from "./mailer";
import * as templates from "./templates";

const pool = new Pool();
const auditLog = new AuditLog();

/**
 * Email the customer of a new order.
 * The audit log and the templates can be replaced in tests.
 */
export async function handleOrderCreated(message, audit = auditLog, tpl = templates) {
  const order = JSON.parse(message.content.toString());
  const found = await pool.query("SELECT email FROM customers WHERE id = $1", [order.customer_id]);
  const email = found.rows[0].email;
  const body = tpl.render("order-created", { order_id: String(order.order_id) });
  await sendEmail(email, "Your order", body);
  audit.recordDelivery(order.order_id, email);
}

/** Subscribe to the order events on an open channel. */
export async function start(channel: Channel): Promise<void> {
  await channel.assertQueue("order.created", { durable: true });
  await channel.consume("order.created", handleOrderCreated);
}
