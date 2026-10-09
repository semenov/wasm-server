-- Catalog schema and seed data for the `catalog` guest app.
CREATE TABLE products (
    id          bigint PRIMARY KEY,
    name        text   NOT NULL,
    price_cents bigint NOT NULL,
    stock       bigint NOT NULL
);

-- 10,000 products. Product 1 is out of stock (for testing 409); the rest have
-- effectively unlimited stock so order load tests never run dry.
INSERT INTO products
SELECT i, 'Product ' || i, 100 + (i * 37) % 10000, CASE WHEN i = 1 THEN 0 ELSE 1000000000 END
FROM generate_series(1, 10000) AS i;

CREATE TABLE orders (
    id         bigserial   PRIMARY KEY,
    product_id bigint      NOT NULL REFERENCES products (id),
    qty        bigint      NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX orders_product_id ON orders (product_id);
