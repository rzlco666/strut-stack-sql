-- Benchmark fixture for MySQL 8.4 (same shape as the SQLite fixture in bench.py).
-- Idempotent: safe to re-run before every benchmark.
DROP TABLE IF EXISTS bench_wide;
DROP TABLE IF EXISTS bench_users;

CREATE TABLE bench_users (
    id         INT PRIMARY KEY,
    email      VARCHAR(255),
    name       VARCHAR(255),
    status     VARCHAR(32),
    created_at VARCHAR(32),
    score      DOUBLE,
    region     VARCHAR(32),
    notes      TEXT
);

INSERT INTO bench_users (id, email, name, status, created_at, score, region, notes)
WITH RECURSIVE seq AS (
    SELECT 1 AS g
    UNION ALL
    SELECT g + 1 FROM seq WHERE g < 1000
)
SELECT g,
       CONCAT('user', g, '@example.com'),
       CONCAT('User ', g),
       IF(g % 3 = 0, 'active', 'idle'),
       CONCAT('2026-01-', LPAD((g % 28) + 1, 2, '0'), 'T12:00:00'),
       (g % 1000) / 10.0,
       ELT((g % 3) + 1, 'us-east', 'eu-west', 'ap-south'),
       CONCAT('benchmark row ', g, ' notes payload')
FROM seq;

CREATE TABLE bench_wide (
    c1 TEXT, c2 TEXT, c3 TEXT, c4 TEXT, c5 TEXT, c6 TEXT, c7 TEXT, c8 TEXT
);

INSERT INTO bench_wide (c1, c2, c3, c4, c5, c6, c7, c8)
WITH RECURSIVE seq AS (
    SELECT 1 AS g
    UNION ALL
    SELECT g + 1 FROM seq WHERE g < 100
)
SELECT CONCAT('r', LPAD(g, 3, '0'), 'c1'),
       CONCAT('r', LPAD(g, 3, '0'), 'c2'),
       CONCAT('r', LPAD(g, 3, '0'), 'c3'),
       CONCAT('r', LPAD(g, 3, '0'), 'c4'),
       CONCAT('r', LPAD(g, 3, '0'), 'c5'),
       CONCAT('r', LPAD(g, 3, '0'), 'c6'),
       CONCAT('r', LPAD(g, 3, '0'), 'c7'),
       CONCAT('r', LPAD(g, 3, '0'), 'c8')
FROM seq;

ANALYZE TABLE bench_users;
ANALYZE TABLE bench_wide;
