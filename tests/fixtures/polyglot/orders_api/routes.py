"""HTTP routes for orders."""
from fastapi import APIRouter, HTTPException

from .events import publish_order_created
from .repo import find_order, insert_order

router = APIRouter(prefix="/orders")


@router.post("")
def create_order(payload: dict):
    """Create an order and announce it."""
    order_id = insert_order(payload["customer_id"], payload["total"])
    publish_order_created(order_id)
    return {"id": order_id}


@router.get("/{order_id}")
def get_order(order_id: int):
    """Return one order."""
    order = find_order(order_id)
    if order is None:
        raise HTTPException(status_code=404)
    return order
