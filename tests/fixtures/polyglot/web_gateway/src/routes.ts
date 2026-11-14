// HTTP routes of the public gateway.
import { Request, Response, Router } from "express";

import { createOrder, fetchOrder } from "./ordersClient";
import { render } from "./views";

export const router = Router();

/** Show one order as an HTML page. */
export async function getOrderHandler(req: Request, res: Response): Promise<void> {
  const order = await fetchOrder(req.params.id);
  res.type("html").end(render("order", order));
}

/** Place an order through the orders service. */
export async function createOrderHandler(req: Request, res: Response): Promise<void> {
  const created = await createOrder(req.body);
  res.status(201).json(created);
}

router.get("/api/orders/:id", getOrderHandler);
router.post("/api/orders", createOrderHandler);
