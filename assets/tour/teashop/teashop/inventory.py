"""How many tins of each tea are on the shelf."""

from teashop.models import Order


class OutOfStock(Exception):
    def __init__(self, sku: str, wanted: int, left: int) -> None:
        super().__init__(f"{sku}: {wanted} wanted, {left} left")
        self.sku = sku


class Inventory:
    """Tins on the shelf, by SKU. An order's tins are set aside before it
    is paid for, and put back if the payment fails."""

    def __init__(self, stock: dict[str, int]) -> None:
        self._stock = dict(stock)

    def left(self, sku: str) -> int:
        return self._stock.get(sku, 0)

    def reserve(self, order: Order) -> None:
        for line in order.lines:
            if self.left(line.tea.sku) < line.quantity:
                raise OutOfStock(line.tea.sku, line.quantity, self.left(line.tea.sku))
        for line in order.lines:
            self._stock[line.tea.sku] -= line.quantity

    def release(self, order: Order) -> None:
        for line in order.lines:
            self._stock[line.tea.sku] = self.left(line.tea.sku) + line.quantity

    def restock(self, sku: str, tins: int) -> None:
        self._stock[sku] = self.left(sku) + tins
