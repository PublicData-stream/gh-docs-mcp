BEGIN;
SELECT pg_advisory_xact_lock(1);
CREATE SCHEMA IF NOT EXISTS foundation AUTHORIZATION gh_docs_migrate;
REVOKE ALL ON SCHEMA foundation FROM PUBLIC;
CREATE TABLE IF NOT EXISTS foundation.probe (
    id integer PRIMARY KEY,
    value text NOT NULL
);
CREATE TABLE IF NOT EXISTS foundation.operator_only (
    id integer PRIMARY KEY
);
INSERT INTO foundation.probe (id, value) VALUES (1, 'phase-zero fixture')
ON CONFLICT (id) DO NOTHING;
GRANT USAGE ON SCHEMA foundation TO gh_docs_serve, gh_docs_sync;
GRANT SELECT ON foundation.probe TO gh_docs_serve;
GRANT SELECT, INSERT, UPDATE, DELETE ON foundation.probe TO gh_docs_sync;
COMMIT;
