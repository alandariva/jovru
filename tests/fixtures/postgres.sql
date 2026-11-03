CREATE TABLE gate_fixture (id INTEGER PRIMARY KEY, amount DECIMAL(20,2), label VARCHAR(80));
INSERT INTO gate_fixture VALUES (1, 123456789.12, 'ação');
CREATE VIEW gate_fixture_view AS SELECT id, amount FROM gate_fixture;
COMMENT ON TABLE gate_fixture IS 'Synthetic gate table';
COMMENT ON COLUMN gate_fixture.label IS 'Free text';
CREATE TABLE gate_child (id INTEGER PRIMARY KEY, fixture_id INTEGER NOT NULL REFERENCES gate_fixture(id), created DATE DEFAULT CURRENT_DATE, tags TEXT[]);
CREATE MATERIALIZED VIEW gate_fixture_totals AS SELECT count(*) AS total FROM gate_fixture;
CREATE TABLE gate_events (id INTEGER NOT NULL, day DATE NOT NULL) PARTITION BY RANGE (day);
CREATE TABLE gate_events_2026 PARTITION OF gate_events FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
