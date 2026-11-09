"""The receipt a customer is sent once an order is paid."""

from teashop import pricing
from teashop.models import Order


def render(order: Order, payment: str) -> str:
    rows = [f"{line.quantity} × {line.tea.name:<24} {line.amount:>8}" for line in order.lines]
    for name, off in pricing.discounts(order):
        rows.append(f"  {name:<26} {-off:>8}")
    rows.append(f"{'Total':<28} {pricing.total(order):>8}")
    rows.append(f"Payment {payment}")
    return "\n".join(rows)


def send(order: Order, payment: str) -> None:
    """Email the receipt. Printed here, as the demo has no mail server."""
    print(f"To: {order.customer.email}\n{render(order, payment)}\n")
