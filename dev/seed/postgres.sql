-- Synthetic data for manual testing (docker-compose.dev.yml). Every value is
-- generated; never add real or production data here.

CREATE SCHEMA catalog;
CREATE SCHEMA sales;

CREATE TYPE sales.order_status AS ENUM ('pending', 'paid', 'shipped', 'cancelled');

CREATE TABLE catalog.product (
    id SERIAL PRIMARY KEY,
    sku TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    price NUMERIC(12, 2) NOT NULL,
    weight_kg REAL,
    active BOOLEAN NOT NULL DEFAULT true,
    tags TEXT[],
    attributes JSONB
);

CREATE TABLE sales.customer (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    email TEXT,
    birth_date DATE,
    created_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE sales.orders (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    customer_id UUID NOT NULL REFERENCES sales.customer (id),
    status sales.order_status NOT NULL,
    ordered_at TIMESTAMP NOT NULL,
    note TEXT
);
CREATE INDEX orders_customer_id ON sales.orders (customer_id);

CREATE TABLE sales.order_item (
    order_id BIGINT NOT NULL REFERENCES sales.orders (id),
    product_id INTEGER NOT NULL REFERENCES catalog.product (id),
    quantity INTEGER NOT NULL CHECK (quantity > 0),
    unit_price NUMERIC(12, 2) NOT NULL,
    PRIMARY KEY (order_id, product_id)
);

-- Large table for row limits, scrolling and cancellation.
CREATE TABLE sales.event_log (
    id BIGINT PRIMARY KEY,
    occurred_at TIMESTAMPTZ NOT NULL,
    level TEXT NOT NULL,
    message TEXT,
    payload JSONB
);
CREATE INDEX event_log_occurred_at ON sales.event_log (occurred_at);

-- One column per common type, with typical, NULL, extreme and long values.
CREATE TABLE public.type_showcase (
    id INTEGER PRIMARY KEY,
    small_int SMALLINT,
    big_int BIGINT,
    exact NUMERIC(38, 10),
    approx DOUBLE PRECISION,
    flag BOOLEAN,
    label TEXT,
    code CHAR(3),
    day DATE,
    clock TIME,
    moment TIMESTAMP,
    moment_tz TIMESTAMPTZ,
    span INTERVAL,
    uid UUID,
    doc JSONB,
    raw BYTEA,
    numbers INTEGER[],
    address INET
);

INSERT INTO catalog.product (sku, name, price, weight_kg, active, tags, attributes)
SELECT
    'SKU-' || lpad(i::text, 4, '0'),
    (ARRAY['Caneta', 'Caderno', 'Mochila', 'Lápis', 'Régua', 'Estojo', 'Agenda', 'Marcador'])[1 + i % 8] || ' ' || i,
    round(5 + (i * 37) % 495 + (i % 100) / 100.0, 2),
    CASE WHEN i % 7 = 0 THEN NULL ELSE (0.05 * (1 + i % 40))::real END,
    i % 11 <> 0,
    ARRAY['escolar', CASE WHEN i % 2 = 0 THEN 'promo' ELSE 'novo' END],
    jsonb_build_object('cor', (ARRAY['azul', 'verde', 'preto'])[1 + i % 3], 'estoque', (i * 13) % 300)
FROM generate_series(1, 50) AS i;

INSERT INTO sales.customer (id, name, email, birth_date, created_at)
SELECT
    md5('customer-' || i)::uuid,
    (ARRAY['Ana', 'Bruno', 'Carla', 'Diego', 'Elisa', 'Fábio', 'Gabriela', 'Hugo', 'Íris', 'João'])[1 + i % 10] || ' ' ||
    (ARRAY['Silva', 'Souza', 'Oliveira', 'Pereira', 'Costa', 'Almeida', 'Gonçalves', 'Araújo'])[1 + (i / 10) % 8],
    CASE WHEN i % 9 = 0 THEN NULL ELSE 'cliente' || i || '@example.com' END,
    date '1960-01-01' + (i * 97) % 15000,
    timestamptz '2024-01-01 00:00:00+00' + make_interval(hours => i * 17)
FROM generate_series(1, 200) AS i;

INSERT INTO sales.orders (customer_id, status, ordered_at, note)
SELECT
    md5('customer-' || (1 + i % 200))::uuid,
    (enum_range(NULL::sales.order_status))[1 + i % 4],
    timestamp '2025-01-01 00:00:00' + make_interval(mins => i * 263),
    CASE WHEN i % 25 = 0 THEN 'Entregar após as 18h' END
FROM generate_series(1, 2000) AS i;

INSERT INTO sales.order_item (order_id, product_id, quantity, unit_price)
SELECT o.id, p.id, 1 + (o.id + p.id) % 5, p.price
FROM sales.orders AS o
JOIN catalog.product AS p ON p.id IN (1 + o.id % 50, 1 + (o.id * 7) % 50);

INSERT INTO sales.event_log (id, occurred_at, level, message, payload)
SELECT
    i,
    timestamptz '2025-01-01 00:00:00+00' + make_interval(secs => i * 30),
    (ARRAY['debug', 'info', 'warn', 'error'])[1 + i % 4],
    'Evento sintético ' || i,
    CASE WHEN i % 10 = 0 THEN NULL ELSE jsonb_build_object('seq', i, 'ok', i % 4 <> 3) END
FROM generate_series(1, 100000) AS i;

INSERT INTO public.type_showcase VALUES
    (1, 42, 9007199254740993, 123456789.0123456789, 3.141592653589793, true,
     'ação e café ☕', 'BRL', '2026-01-31', '23:59:59.123456', '2026-01-31 23:59:59',
     '2026-01-31 23:59:59-03', '1 day 02:03:04', md5('showcase-1')::uuid,
     '{"a": 1, "nested": {"b": [1, 2, null]}}', '\xdeadbeef', '{1,2,3}', '192.168.0.1'),
    (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
     NULL, NULL, NULL, NULL),
    (3, -32768, -9223372036854775808, 9999999999999999999999999999.9999999999, 1e308, false,
     '', '   ', '0001-01-01', '00:00:00', '1970-01-01 00:00:00',
     '2038-01-19 03:14:08+00', '-1 year', '00000000-0000-0000-0000-000000000000',
     '[]', '\x', '{}', '::1'),
    (4, 32767, 9223372036854775807, -0.0000000001, 'NaN', true,
     repeat('Texto longo para testar truncamento. ', 200), 'ÇÃO', '9999-12-31', '12:00:00',
     'infinity', '-infinity', '10 years 3 mons', md5('showcase-4')::uuid,
     '{"emoji": "😀", "quote": "aspas \"duplas\""}', decode(repeat('ab', 512), 'hex'),
     '{-1,0,NULL}', '10.0.0.0/8'),
    (5, 0, 0, 0, -0.5, false,
     E'linha 1\nlinha 2\ttab', 'ABC', '2000-02-29', '06:30:00', '2000-02-29 06:30:00',
     '2000-02-29 06:30:00+05:30', '00:00:00.000001', md5('showcase-5')::uuid,
     '"texto json"', '\x00ff', '{}', '2001:db8::1');

CREATE VIEW sales.order_summary AS
SELECT o.id, c.name AS customer, o.status, o.ordered_at, sum(i.quantity * i.unit_price) AS total
FROM sales.orders AS o
JOIN sales.customer AS c ON c.id = o.customer_id
JOIN sales.order_item AS i ON i.order_id = o.id
GROUP BY o.id, c.name;

CREATE FUNCTION sales.customer_total(customer UUID) RETURNS NUMERIC
LANGUAGE sql STABLE AS $$
    SELECT coalesce(sum(i.quantity * i.unit_price), 0)
    FROM sales.orders AS o
    JOIN sales.order_item AS i ON i.order_id = o.id
    WHERE o.customer_id = customer
$$;

ANALYZE;
