CREATE TABLE gate_fixture (id INTEGER PRIMARY KEY, amount DECIMAL(20,2), label VARCHAR(80));
INSERT INTO gate_fixture VALUES (1, 123456789.12, 'ação');
CREATE VIEW gate_fixture_view AS SELECT id, amount FROM gate_fixture;
