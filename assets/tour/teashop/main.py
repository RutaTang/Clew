"""Place two orders, one of which is declined. Run it, or debug it."""

from teashop.catalog import Catalog
from teashop.checkout import checkout
from teashop.inventory import Inventory
from teashop.models import Customer, Order
from teashop.payments import PaymentDeclined


def main() -> None:
    catalog = Catalog()
    inventory = Inventory({"SEN-01": 10, "DAR-02": 3, "EAR-05": 8})

    order = Order(Customer("ada@example.com", "Ada", member=True))
    order.add(catalog.find("SEN-01"), 3)
    order.add(catalog.find("EAR-05"), 2)
    checkout(order, inventory)

    declined = Order(Customer("declined@example.com", "Bo"))
    declined.add(catalog.find("DAR-02"))
    try:
        checkout(declined, inventory)
    except PaymentDeclined as e:
        print(f"Declined: {e}; Darjeeling left: {inventory.left('DAR-02')}")


if __name__ == "__main__":
    main()
