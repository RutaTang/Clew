"""The shop's data: teas, customers, and the orders they place."""

from dataclasses import dataclass, field
from decimal import Decimal


@dataclass(frozen=True)
class Tea:
    """A tea on the shelf, sold by the 100 g tin."""

    sku: str
    name: str
    origin: str
    price: Decimal


@dataclass
class Customer:
    """Someone who buys tea. Members get a discount on every order."""

    email: str
    name: str
    member: bool = False


@dataclass
class OrderLine:
    """One tea in an order, and how many tins of it."""

    tea: Tea
    quantity: int

    @property
    def amount(self) -> Decimal:
        return self.tea.price * self.quantity


@dataclass
class Order:
    """What a customer is buying. Its total is worked out by `pricing`."""

    customer: Customer
    lines: list[OrderLine] = field(default_factory=list)
    paid: bool = False

    def add(self, tea: Tea, quantity: int = 1) -> None:
        for line in self.lines:
            if line.tea.sku == tea.sku:
                line.quantity += quantity
                return
        self.lines.append(OrderLine(tea, quantity))

    def tins(self) -> int:
        return sum(line.quantity for line in self.lines)
