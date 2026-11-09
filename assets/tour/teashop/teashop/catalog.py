"""The teas the shop sells, and finding one."""

from decimal import Decimal

from teashop.models import Tea

TEAS = [
    Tea("SEN-01", "Sencha", "Shizuoka, Japan", Decimal("12.50")),
    Tea("DAR-02", "Darjeeling First Flush", "Darjeeling, India", Decimal("16.00")),
    Tea("OOL-03", "Tieguanyin", "Anxi, China", Decimal("14.75")),
    Tea("ROO-04", "Rooibos", "Cederberg, South Africa", Decimal("8.25")),
    Tea("EAR-05", "Earl Grey", "Blended in-house", Decimal("9.90")),
]


class Catalog:
    """Looks teas up by their SKU or their name."""

    def __init__(self, teas: list[Tea] | None = None) -> None:
        self._by_sku = {tea.sku: tea for tea in (teas or TEAS)}

    def find(self, sku: str) -> Tea:
        try:
            return self._by_sku[sku]
        except KeyError:
            raise LookupError(f"no tea with SKU {sku}") from None

    def search(self, words: str) -> list[Tea]:
        """Teas whose name or origin holds every one of `words`."""
        wanted = words.lower().split()
        return [
            tea
            for tea in self._by_sku.values()
            if all(w in f"{tea.name} {tea.origin}".lower() for w in wanted)
        ]
