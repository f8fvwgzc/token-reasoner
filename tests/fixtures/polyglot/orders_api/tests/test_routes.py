"""Route tests."""
from orders_api.routes import create_order


def test_create_order():
    response = create_order({"customer_id": 1, "total": 25})
    assert "id" in response
