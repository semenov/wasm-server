-- Catalog schema and seed data for SQLite (`--db-init main=bench/catalog-sqlite.sql`).
-- Idempotent: safe to run at every start.
CREATE TABLE IF NOT EXISTS products (
    id          INTEGER PRIMARY KEY,
    name        TEXT    NOT NULL,
    price_cents INTEGER NOT NULL,
    stock       INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS orders (
    id         INTEGER PRIMARY KEY,
    product_id INTEGER NOT NULL REFERENCES products (id),
    qty        INTEGER NOT NULL,
    created_at TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS orders_product_id ON orders (product_id);

-- 10,000 products; product 1 is out of stock.
INSERT OR IGNORE INTO products (id, name, price_cents, stock)
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10000)
SELECT i, 'Product ' || i, 100 + (i * 37) % 10000, CASE WHEN i = 1 THEN 0 ELSE 1000000000 END FROM n;
