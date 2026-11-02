-- Synthetic data for manual testing (docker-compose.dev.yml). Every value is
-- generated; never add real or production data here.
-- MySQL has no schemas inside a database, so catalog and sales are databases.

-- The entrypoint client reads this file as latin1 unless told otherwise.
SET NAMES utf8mb4;

CREATE DATABASE catalog;
CREATE DATABASE sales;
GRANT ALL PRIVILEGES ON catalog.* TO 'jykre'@'%';
GRANT ALL PRIVILEGES ON sales.* TO 'jykre'@'%';

SET SESSION cte_max_recursion_depth = 100000;

CREATE TABLE catalog.product (
    id INT AUTO_INCREMENT PRIMARY KEY,
    sku VARCHAR(20) NOT NULL UNIQUE,
    name VARCHAR(100) NOT NULL,
    price DECIMAL(12, 2) NOT NULL,
    weight_kg FLOAT,
    active BOOLEAN NOT NULL DEFAULT TRUE,
    tags SET('escolar', 'promo', 'novo'),
    attributes JSON
);

CREATE TABLE sales.customer (
    id CHAR(36) PRIMARY KEY,
    name VARCHAR(100) NOT NULL,
    email VARCHAR(100),
    birth_date DATE,
    created_at TIMESTAMP NOT NULL
);

CREATE TABLE sales.orders (
    id BIGINT AUTO_INCREMENT PRIMARY KEY,
    customer_id CHAR(36) NOT NULL,
    status ENUM('pending', 'paid', 'shipped', 'cancelled') NOT NULL,
    ordered_at DATETIME NOT NULL,
    note VARCHAR(200),
    FOREIGN KEY (customer_id) REFERENCES sales.customer (id)
);

CREATE TABLE sales.order_item (
    order_id BIGINT NOT NULL,
    product_id INT NOT NULL,
    quantity INT NOT NULL CHECK (quantity > 0),
    unit_price DECIMAL(12, 2) NOT NULL,
    PRIMARY KEY (order_id, product_id),
    FOREIGN KEY (order_id) REFERENCES sales.orders (id),
    FOREIGN KEY (product_id) REFERENCES catalog.product (id)
);

-- Large table for row limits, scrolling and cancellation.
CREATE TABLE sales.event_log (
    id BIGINT PRIMARY KEY,
    occurred_at DATETIME(3) NOT NULL,
    level VARCHAR(10) NOT NULL,
    message VARCHAR(100),
    payload JSON,
    INDEX event_log_occurred_at (occurred_at)
);

-- One column per common type, with typical, NULL, extreme and long values.
CREATE TABLE jykre.type_showcase (
    id INT PRIMARY KEY,
    tiny_int TINYINT,
    small_int SMALLINT,
    big_int BIGINT,
    unsigned_big BIGINT UNSIGNED,
    exact DECIMAL(38, 10),
    approx DOUBLE,
    flag BOOLEAN,
    label TEXT,
    code CHAR(3),
    day DATE,
    clock TIME(6),
    moment DATETIME(6),
    moment_ts TIMESTAMP NULL,
    year_only YEAR,
    uid CHAR(36),
    doc JSON,
    raw VARBINARY(1024),
    bits BIT(8),
    mood ENUM('feliz', 'neutro', 'triste')
);

INSERT INTO catalog.product (sku, name, price, weight_kg, active, tags, attributes)
WITH RECURSIVE seq (i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 50)
SELECT
    CONCAT('SKU-', LPAD(i, 4, '0')),
    CONCAT(ELT(1 + i % 8, 'Caneta', 'Caderno', 'Mochila', 'Lápis', 'Régua', 'Estojo', 'Agenda', 'Marcador'), ' ', i),
    ROUND(5 + (i * 37) % 495 + (i % 100) / 100, 2),
    IF(i % 7 = 0, NULL, 0.05 * (1 + i % 40)),
    i % 11 <> 0,
    IF(i % 2 = 0, 'escolar,promo', 'escolar,novo'),
    JSON_OBJECT('cor', ELT(1 + i % 3, 'azul', 'verde', 'preto'), 'estoque', (i * 13) % 300)
FROM seq;

INSERT INTO sales.customer (id, name, email, birth_date, created_at)
WITH RECURSIVE seq (i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 200)
SELECT
    INSERT(INSERT(INSERT(INSERT(MD5(CONCAT('customer-', i)), 21, 0, '-'), 17, 0, '-'), 13, 0, '-'), 9, 0, '-'),
    CONCAT(
        ELT(1 + i % 10, 'Ana', 'Bruno', 'Carla', 'Diego', 'Elisa', 'Fábio', 'Gabriela', 'Hugo', 'Íris', 'João'), ' ',
        ELT(1 + (i DIV 10) % 8, 'Silva', 'Souza', 'Oliveira', 'Pereira', 'Costa', 'Almeida', 'Gonçalves', 'Araújo')),
    IF(i % 9 = 0, NULL, CONCAT('cliente', i, '@example.com')),
    DATE '1960-01-01' + INTERVAL ((i * 97) % 15000) DAY,
    TIMESTAMP '2024-01-01 00:00:00' + INTERVAL (i * 17) HOUR
FROM seq;

INSERT INTO sales.orders (customer_id, status, ordered_at, note)
WITH RECURSIVE seq (i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 2000)
SELECT
    INSERT(INSERT(INSERT(INSERT(MD5(CONCAT('customer-', 1 + i % 200)), 21, 0, '-'), 17, 0, '-'), 13, 0, '-'), 9, 0, '-'),
    ELT(1 + i % 4, 'pending', 'paid', 'shipped', 'cancelled'),
    TIMESTAMP '2025-01-01 00:00:00' + INTERVAL (i * 263) MINUTE,
    IF(i % 25 = 0, 'Entregar após as 18h', NULL)
FROM seq;

INSERT INTO sales.order_item (order_id, product_id, quantity, unit_price)
SELECT o.id, p.id, 1 + (o.id + p.id) % 5, p.price
FROM sales.orders AS o
JOIN catalog.product AS p ON p.id IN (1 + o.id % 50, 1 + (o.id * 7) % 50);

INSERT INTO sales.event_log (id, occurred_at, level, message, payload)
WITH RECURSIVE seq (i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 100000)
SELECT
    i,
    TIMESTAMP '2025-01-01 00:00:00' + INTERVAL (i * 30) SECOND,
    ELT(1 + i % 4, 'debug', 'info', 'warn', 'error'),
    CONCAT('Evento sintético ', i),
    IF(i % 10 = 0, NULL, JSON_OBJECT('seq', i, 'ok', CAST(IF(i % 4 <> 3, 'true', 'false') AS JSON)))
FROM seq;

INSERT INTO jykre.type_showcase VALUES
    (1, 7, 42, 9007199254740993, 18446744073709551615, 123456789.0123456789, 3.141592653589793,
     TRUE, 'ação e café ☕', 'BRL', '2026-01-31', '23:59:59.123456', '2026-01-31 23:59:59.123456',
     '2026-01-31 23:59:59', 2026, '6f1ed002-ab5e-4b0b-a9a3-bfd8e3a8b5c1',
     '{"a": 1, "nested": {"b": [1, 2, null]}}', X'DEADBEEF', b'10101010', 'feliz'),
    (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
     NULL, NULL, NULL, NULL, NULL),
    (3, -128, -32768, -9223372036854775808, 0, 9999999999999999999999999999.9999999999, 1e308,
     FALSE, '', '   ', '1000-01-01', '-838:59:59', '1000-01-01 00:00:00',
     '1970-01-01 00:00:01', 1901, '00000000-0000-0000-0000-000000000000',
     '[]', X'', b'00000000', 'neutro'),
    (4, 127, 32767, 9223372036854775807, 1, -0.0000000001, -0.5,
     TRUE, REPEAT('Texto longo para testar truncamento. ', 200), 'ÇÃO', '9999-12-31', '838:59:59',
     '9999-12-31 23:59:59.999999', '2038-01-19 03:14:07', 2155, 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11',
     '{"emoji": "😀", "quote": "aspas \\"duplas\\""}', UNHEX(REPEAT('AB', 512)), b'11111111', 'triste'),
    (5, 0, 0, 0, 0, 0, 0,
     FALSE, 'linha 1\nlinha 2\ttab', 'ABC', '2000-02-29', '06:30:00', '2000-02-29 06:30:00',
     '2000-02-29 06:30:00', 2000, '123e4567-e89b-12d3-a456-426614174000',
     '"texto json"', X'00FF', b'00000001', 'feliz');

CREATE VIEW sales.order_summary AS
SELECT o.id, c.name AS customer, o.status, o.ordered_at, SUM(i.quantity * i.unit_price) AS total
FROM sales.orders AS o
JOIN sales.customer AS c ON c.id = o.customer_id
JOIN sales.order_item AS i ON i.order_id = o.id
GROUP BY o.id, c.name;

DELIMITER //
CREATE FUNCTION sales.customer_total(customer CHAR(36)) RETURNS DECIMAL(14, 2)
READS SQL DATA
BEGIN
    RETURN (
        SELECT COALESCE(SUM(i.quantity * i.unit_price), 0)
        FROM sales.orders AS o
        JOIN sales.order_item AS i ON i.order_id = o.id
        WHERE o.customer_id = customer
    );
END //
DELIMITER ;
