CREATE TABLE gate_fixture (id INTEGER PRIMARY KEY, amount DECIMAL(20,2), label VARCHAR(80));
INSERT INTO gate_fixture VALUES (1, 123456789.12, 'ação');
CREATE VIEW gate_fixture_view AS SELECT id, amount FROM gate_fixture;
CREATE TABLE gate_child (id INTEGER PRIMARY KEY, fixture_id INTEGER NOT NULL, created VARCHAR(10) DEFAULT 'none' COMMENT 'Synthetic default', CONSTRAINT gate_child_fixture FOREIGN KEY (fixture_id) REFERENCES gate_fixture(id)) COMMENT='Synthetic child';
-- What an inspection lists beyond the refreshed tree.
CREATE INDEX gate_fixture_label ON gate_fixture (label);
ALTER TABLE gate_fixture ADD CONSTRAINT gate_fixture_amount CHECK (amount >= 0);
CREATE TRIGGER gate_fixture_touch BEFORE UPDATE ON gate_fixture FOR EACH ROW SET NEW.label = NEW.label;
CREATE PROCEDURE gate_touch(IN steps INT, OUT total INT) SET total = steps + 1;
-- A login that may not use one database, for permission warnings.
CREATE DATABASE gate_private;
CREATE USER 'gate_reader'@'%' IDENTIFIED BY 'jykre-test';
GRANT SELECT ON jykre.* TO 'gate_reader'@'%';
-- A login that never signs in over TLS, so the server never caches its
-- caching_sha2_password hash: without TLS it always needs the RSA exchange.
CREATE USER 'gate_plain'@'%' IDENTIFIED BY 'jykre-test';
GRANT SELECT ON jykre.* TO 'gate_plain'@'%';
