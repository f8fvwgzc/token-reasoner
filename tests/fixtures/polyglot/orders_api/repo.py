"""Order persistence."""
import psycopg2

from .models import Customer

DSN = "dbname=orders"


def connect():
    return psycopg2.connect(DSN)


def insert_order(customer_id, total):
    """Store a new order and return its id."""
    with connect() as conn, conn.cursor() as cur:
        cur.execute(
            "INSERT INTO orders (customer_id, total) VALUES (%s, %s) RETURNING id",
            (customer_id, total),
        )
        return cur.fetchone()[0]


def find_order(order_id):
    """Load one order by id."""
    with connect() as conn, conn.cursor() as cur:
        cur.execute("SELECT id, customer_id, total FROM orders WHERE id = %s", (order_id,))
        return cur.fetchone()


def find_customer(session, customer_id):
    """Load the customer an order belongs to."""
    return session.query(Customer).get(customer_id)
