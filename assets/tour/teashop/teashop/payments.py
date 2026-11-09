"""Taking payment. A stand-in for a real card processor."""

import hashlib
from decimal import Decimal

from teashop.models import Customer


class PaymentDeclined(Exception):
    pass


# A card that is declined, as a processor's test card is.
DECLINED = {"declined@example.com"}


def charge(customer: Customer, amount: Decimal) -> str:
    """Charge `customer`; the processor's reference for the payment."""
    if customer.email in DECLINED:
        raise PaymentDeclined(f"card declined for {customer.email}")
    return "pay_" + hashlib.sha256(f"{customer.email}:{amount}".encode()).hexdigest()[:10]
