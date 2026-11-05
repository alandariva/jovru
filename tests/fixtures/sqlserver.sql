-- Run by the gate as the server administrator, with sqlcmd (GO ends a batch).
-- The login keeps the public fixture password: its policy check is off.
CREATE LOGIN jykre WITH PASSWORD = 'jykre-test', CHECK_POLICY = OFF, DEFAULT_DATABASE = master;
CREATE LOGIN gate_reader WITH PASSWORD = 'jykre-test', CHECK_POLICY = OFF;
GO
CREATE DATABASE jykre;
GO
ALTER LOGIN jykre WITH DEFAULT_DATABASE = jykre;
GO
USE jykre;
GO
CREATE USER jykre FOR LOGIN jykre;
ALTER ROLE db_owner ADD MEMBER jykre;
CREATE USER gate_reader FOR LOGIN gate_reader;
ALTER ROLE db_datareader ADD MEMBER gate_reader;
GO
CREATE TABLE gate_fixture (id INTEGER PRIMARY KEY, amount DECIMAL(20,2), label NVARCHAR(80));
INSERT INTO gate_fixture VALUES (1, 123456789.12, N'ação');
EXEC sp_addextendedproperty N'MS_Description', N'Synthetic gate table', N'SCHEMA', N'dbo', N'TABLE', N'gate_fixture';
EXEC sp_addextendedproperty N'MS_Description', N'Free text', N'SCHEMA', N'dbo', N'TABLE', N'gate_fixture', N'COLUMN', N'label';
GO
CREATE VIEW gate_fixture_view AS SELECT id, amount FROM gate_fixture;
GO
CREATE TABLE gate_child (id INTEGER PRIMARY KEY, fixture_id INTEGER NOT NULL, created VARCHAR(10) CONSTRAINT gate_child_created DEFAULT 'none', CONSTRAINT gate_child_fixture FOREIGN KEY (fixture_id) REFERENCES gate_fixture(id));
GO
-- What an inspection lists beyond the refreshed tree.
CREATE INDEX gate_fixture_label ON gate_fixture (label);
ALTER TABLE gate_fixture ADD CONSTRAINT gate_fixture_amount CHECK (amount >= 0);
CREATE SEQUENCE gate_counter START WITH 10 INCREMENT BY 5;
CREATE TYPE gate_code FROM NVARCHAR(12) NOT NULL;
GO
CREATE TRIGGER gate_fixture_touch ON gate_fixture AFTER UPDATE AS SET NOCOUNT ON;
GO
CREATE PROCEDURE gate_touch @steps INT, @total INT OUTPUT AS SET @total = @steps + 1;
GO
CREATE SCHEMA gate_archive;
GO
CREATE TABLE gate_archive.gate_events (id INTEGER NOT NULL, day DATE NOT NULL);
GO
-- A database the reader may not use, for permission warnings.
CREATE DATABASE gate_private;
GO
