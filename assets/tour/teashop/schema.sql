-- Where the shop keeps its teas, customers and orders.

CREATE TABLE teas (
    sku        TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    origin     TEXT NOT NULL,
    price      NUMERIC(8, 2) NOT NULL,
    tins_left  INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE customers (
    email   TEXT PRIMARY KEY,
    name    TEXT NOT NULL,
    member  BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE orders (
    id         INTEGER PRIMARY KEY,
    customer   TEXT NOT NULL REFERENCES customers (email),
    payment    TEXT,
    placed_at  TIMESTAMP NOT NULL
);

CREATE TABLE order_lines (
    order_id  INTEGER NOT NULL REFERENCES orders (id),
    sku       TEXT NOT NULL REFERENCES teas (sku),
    quantity  INTEGER NOT NULL CHECK (quantity > 0),
    PRIMARY KEY (order_id, sku)
);
