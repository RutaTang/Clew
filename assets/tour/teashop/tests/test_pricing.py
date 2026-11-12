from decimal import Decimal

from teashop.catalog import Catalog
from teashop.models import Customer, Order
from teashop.pricing import total

catalog = Catalog()


def test_member_pays_less():
    guest = Order(Customer("g@example.com", "Guest"))
    member = Order(Customer("m@example.com", "Member", member=True))
    for order in (guest, member):
        order.add(catalog.find("SEN-01"), 2)
    assert total(member) < total(guest)


def test_tax_is_on_the_discounted_amount():
    order = Order(Customer("m@example.com", "Member", member=True))
    order.add(catalog.find("DAR-02"))
    # 16.00 less 10% is 14.40; 8% tax on that is 1.152.
    assert total(order) == Decimal("15.55")
