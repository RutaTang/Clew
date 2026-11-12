"""Checking out: the order's tins set aside, paid for, and a receipt sent."""

from teashop import payments, pricing, receipts
from teashop.inventory import Inventory
from teashop.models import Order


def checkout(order: Order, inventory: Inventory) -> str:
    """Pay for `order`; the payment's reference.

    The tins are set aside first, so two customers cannot buy the last
    one; if the card is declined they go back on the shelf.
    """
    if not order.lines:
        raise ValueError("an empty order cannot be checked out")
    inventory.reserve(order)
    try:
        payment = payments.charge(order.customer, pricing.total(order))
    except payments.PaymentDeclined:
        inventory.release(order)
        raise
    order.paid = True
    receipts.send(order, payment)
    return payment
