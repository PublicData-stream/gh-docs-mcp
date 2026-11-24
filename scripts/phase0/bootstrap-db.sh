#!/bin/sh
set -eu

SERVE_PASSWORD=$(cat /run/secrets/serve-password)
SYNC_PASSWORD=$(cat /run/secrets/sync-password)
MIGRATE_PASSWORD=$(cat /run/secrets/migrate-password)
export SERVE_PASSWORD SYNC_PASSWORD MIGRATE_PASSWORD
psql -X -v ON_ERROR_STOP=1 --username postgres --dbname gh_docs <<'SQL'
\getenv serve_password SERVE_PASSWORD
\getenv sync_password SYNC_PASSWORD
\getenv migrate_password MIGRATE_PASSWORD
BEGIN;
REVOKE ALL ON DATABASE gh_docs FROM PUBLIC;
REVOKE ALL ON SCHEMA public FROM PUBLIC;
CREATE ROLE gh_docs_serve LOGIN NOINHERIT PASSWORD :'serve_password';
CREATE ROLE gh_docs_sync LOGIN NOINHERIT PASSWORD :'sync_password';
CREATE ROLE gh_docs_migrate LOGIN NOINHERIT PASSWORD :'migrate_password';
GRANT CONNECT ON DATABASE gh_docs TO gh_docs_serve, gh_docs_sync, gh_docs_migrate;
GRANT CREATE ON DATABASE gh_docs TO gh_docs_migrate;
ALTER ROLE gh_docs_serve SET default_transaction_read_only = on;
COMMIT;
SQL
