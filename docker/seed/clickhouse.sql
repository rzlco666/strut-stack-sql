-- ClickHouse seed data for docker-compose (local integration testing).
CREATE DATABASE IF NOT EXISTS demo;

CREATE TABLE IF NOT EXISTS demo.events
(
    id UInt64,
    name String,
    amount Decimal(18, 2),
    created_at DateTime
)
ENGINE = MergeTree
ORDER BY id;

INSERT INTO demo.events VALUES
    (1, 'alpha', 12.50, '2024-01-02 03:04:05'),
    (2, 'beta', 7.25, '2024-01-03 04:05:06');
