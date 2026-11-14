// Client for the orders service.
import axios from "axios";

const ORDERS_URL = process.env.ORDERS_URL ?? "http://orders-api";

export interface NewOrder {
  customer_id: number;
  total: number;
}

/** Create an order and return what the orders service answers. */
export async function createOrder(body: NewOrder) {
  const response = await axios.post("http://orders-api/orders", body);
  return response.data;
}

/** Load one order by id. */
export async function fetchOrder(id: string) {
  const response = await axios.get(`${ORDERS_URL}/orders/${id}`);
  return response.data;
}
