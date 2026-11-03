-- Demo schema for strut-stack-sql (PostgreSQL)
CREATE SCHEMA IF NOT EXISTS demo;
SET search_path TO demo;

CREATE TABLE users (
    id          SERIAL PRIMARY KEY,
    email       TEXT NOT NULL UNIQUE,
    name        TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE products (
    id          SERIAL PRIMARY KEY,
    sku         TEXT NOT NULL UNIQUE,
    name        TEXT NOT NULL,
    price_cents INTEGER NOT NULL CHECK (price_cents >= 0)
);

CREATE TABLE orders (
    id          SERIAL PRIMARY KEY,
    user_id     INTEGER NOT NULL REFERENCES users(id),
    product_id  INTEGER NOT NULL REFERENCES products(id),
    quantity    INTEGER NOT NULL CHECK (quantity > 0),
    status      TEXT NOT NULL DEFAULT 'pending',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_orders_user_id ON orders(user_id);
CREATE INDEX idx_orders_status ON orders(status);

INSERT INTO users (email, name)
SELECT
    'user' || g || '@example.com',
    'User ' || g
FROM generate_series(1, 200) AS g;

INSERT INTO products (sku, name, price_cents)
SELECT
    'SKU-' || LPAD(g::text, 5, '0'),
    'Product ' || g,
    (g % 50 + 1) * 100
FROM generate_series(1, 50) AS g;

INSERT INTO orders (user_id, product_id, quantity, status)
SELECT
    (g % 200) + 1,
    (g % 50) + 1,
    (g % 5) + 1,
    CASE WHEN g % 3 = 0 THEN 'shipped' WHEN g % 3 = 1 THEN 'pending' ELSE 'cancelled' END
FROM generate_series(1, 1000) AS g;
