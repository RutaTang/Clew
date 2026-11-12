"""The shop's web API, as a Flask app."""

from flask import Flask, jsonify, request

from teashop.catalog import Catalog
from teashop.checkout import checkout
from teashop.inventory import Inventory
from teashop.models import Customer, Order

app = Flask(__name__)
catalog = Catalog()
inventory = Inventory({"SEN-01": 40, "DAR-02": 12, "OOL-03": 20, "ROO-04": 60, "EAR-05": 35})


@app.get("/teas")
def list_teas():
    found = catalog.search(request.args.get("q", ""))
    return jsonify([{"sku": t.sku, "name": t.name, "price": str(t.price)} for t in found])


@app.get("/teas/<sku>")
def get_tea(sku: str):
    tea = catalog.find(sku)
    return jsonify({"sku": tea.sku, "name": tea.name, "left": inventory.left(sku)})


@app.post("/orders")
def place_order():
    body = request.get_json()
    order = Order(Customer(body["email"], body["name"], body.get("member", False)))
    for item in body["items"]:
        order.add(catalog.find(item["sku"]), item.get("quantity", 1))
    payment = checkout(order, inventory)
    return jsonify({"payment": payment}), 201
