-- Benchmark fixture for PostgreSQL (same shape as the SQLite fixture in bench.py).
-- Idempotent: safe to re-run before every benchmark.
DROP TABLE IF EXISTS public.bench_wide;
DROP TABLE IF EXISTS public.bench_users;

CREATE TABLE public.bench_users (
    id         INTEGER PRIMARY KEY,
    email      TEXT,
    name       TEXT,
    status     TEXT,
    created_at TEXT,
    score      DOUBLE PRECISION,
    region     TEXT,
    notes      TEXT
);

INSERT INTO public.bench_users (id, email, name, status, created_at, score, region, notes)
SELECT g,
       'user' || g || '@example.com',
       'User ' || g,
       CASE WHEN g % 3 = 0 THEN 'active' ELSE 'idle' END,
       '2026-01-' || LPAD(((g % 28) + 1)::text, 2, '0') || 'T12:00:00',
       (g % 1000) / 10.0,
       (ARRAY['us-east','eu-west','ap-south'])[(g % 3) + 1],
       'benchmark row ' || g || ' notes payload'
FROM generate_series(1, 1000) AS g;

CREATE TABLE public.bench_wide (
    c1 TEXT, c2 TEXT, c3 TEXT, c4 TEXT, c5 TEXT, c6 TEXT, c7 TEXT, c8 TEXT
);

INSERT INTO public.bench_wide (c1, c2, c3, c4, c5, c6, c7, c8)
SELECT 'r' || LPAD(g::text, 3, '0') || 'c1',
       'r' || LPAD(g::text, 3, '0') || 'c2',
       'r' || LPAD(g::text, 3, '0') || 'c3',
       'r' || LPAD(g::text, 3, '0') || 'c4',
       'r' || LPAD(g::text, 3, '0') || 'c5',
       'r' || LPAD(g::text, 3, '0') || 'c6',
       'r' || LPAD(g::text, 3, '0') || 'c7',
       'r' || LPAD(g::text, 3, '0') || 'c8'
FROM generate_series(1, 100) AS g;

ANALYZE public.bench_users;
ANALYZE public.bench_wide;
