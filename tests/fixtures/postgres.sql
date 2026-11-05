CREATE TABLE gate_fixture (id INTEGER PRIMARY KEY, amount DECIMAL(20,2), label VARCHAR(80));
INSERT INTO gate_fixture VALUES (1, 123456789.12, 'ação');
CREATE VIEW gate_fixture_view AS SELECT id, amount FROM gate_fixture;
COMMENT ON TABLE gate_fixture IS 'Synthetic gate table';
COMMENT ON COLUMN gate_fixture.label IS 'Free text';
CREATE TABLE gate_child (id INTEGER PRIMARY KEY, fixture_id INTEGER NOT NULL REFERENCES gate_fixture(id), created DATE DEFAULT CURRENT_DATE, tags TEXT[]);
CREATE MATERIALIZED VIEW gate_fixture_totals AS SELECT count(*) AS total FROM gate_fixture;
CREATE TABLE gate_events (id INTEGER NOT NULL, day DATE NOT NULL) PARTITION BY RANGE (day);
CREATE TABLE gate_events_2026 PARTITION OF gate_events FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
-- A partition kept in another schema than its root table.
CREATE SCHEMA gate_archive;
CREATE TABLE gate_archive.gate_events_2025 PARTITION OF gate_events FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');
-- What an inspection lists beyond the refreshed tree.
CREATE INDEX gate_fixture_label ON gate_fixture (label);
ALTER TABLE gate_fixture ADD CONSTRAINT gate_fixture_amount CHECK (amount >= 0);
CREATE FUNCTION gate_touch(day DATE, steps INTEGER DEFAULT 1) RETURNS DATE LANGUAGE sql AS $$ SELECT day + steps $$;
CREATE FUNCTION gate_touch_trigger() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
CREATE TRIGGER gate_fixture_touch BEFORE UPDATE ON gate_fixture FOR EACH ROW EXECUTE FUNCTION gate_touch_trigger();
CREATE SEQUENCE gate_counter START 10 INCREMENT 5;
CREATE TYPE gate_mood AS ENUM ('calm', 'busy');
-- A login that may not use one schema, for permission warnings.
CREATE ROLE gate_reader LOGIN PASSWORD 'jykre-test';
CREATE SCHEMA gate_private;
CREATE TABLE gate_private.hidden (id INTEGER);
REVOKE ALL ON SCHEMA gate_private FROM PUBLIC;
