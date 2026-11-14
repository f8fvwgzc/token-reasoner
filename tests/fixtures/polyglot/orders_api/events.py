"""Order events on RabbitMQ."""
import json

import pika


def publish_order_created(order_id):
    """Tell the other services that an order exists."""
    connection = pika.BlockingConnection(pika.ConnectionParameters("rabbitmq"))
    channel = connection.channel()
    channel.queue_declare(queue="order.created", durable=True)
    channel.basic_publish(
        exchange="",
        routing_key="order.created",
        body=json.dumps({"order_id": order_id}),
    )
    connection.close()
