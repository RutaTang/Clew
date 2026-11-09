"""Discounts an order can earn. Each one says how much it takes off."""

from decimal import Decimal

from teashop.models import Order


class Discount:
    """A rule that takes something off an order's subtotal."""

    name = "discount"

    def amount_off(self, order: Order, subtotal: Decimal) -> Decimal:
        raise NotImplementedError


class MemberDiscount(Discount):
    """Members save 10% on everything."""

    name = "member 10%"

    def amount_off(self, order: Order, subtotal: Decimal) -> Decimal:
        return subtotal * Decimal("0.10") if order.customer.member else Decimal(0)


class BulkDiscount(Discount):
    """Five tins or more: the cheapest tin is free."""

    name = "fifth tin free"

    def amount_off(self, order: Order, subtotal: Decimal) -> Decimal:
        if order.tins() < 5:
            return Decimal(0)
        return min(line.tea.price for line in order.lines)


ALL = [MemberDiscount(), BulkDiscount()]
