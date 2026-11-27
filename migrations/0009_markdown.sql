CREATE TABLE git_catalog.markdown_approvals (
    source_id uuid PRIMARY KEY,
    policy_version bigint NOT NULL CHECK (policy_version > 0),
    repository_id uuid REFERENCES git_catalog.repositories,
    ref_name bytea,
    policy jsonb NOT NULL,
    revoked boolean NOT NULL DEFAULT false,
    next_due timestamptz NOT NULL DEFAULT clock_timestamp(),
    CHECK ((repository_id IS NULL) = (ref_name IS NULL))
);

CREATE TABLE git_catalog.markdown_jobs (
    job_id uuid PRIMARY KEY,
    source_id uuid NOT NULL REFERENCES git_catalog.markdown_approvals,
    policy_version bigint NOT NULL,
    repository_id uuid,
    generation_id bigint,
    commit_oid bytea,
    git_policy_version bigint,
    state text NOT NULL CHECK (state IN ('queued','running','succeeded','failed','cancelled')),
    attempts integer NOT NULL DEFAULT 0,
    due timestamptz NOT NULL DEFAULT clock_timestamp(),
    lease_until timestamptz,
    reason text,
    CHECK (num_nonnulls(repository_id, generation_id, commit_oid, git_policy_version) IN (0,4))
);
CREATE UNIQUE INDEX markdown_open_job ON git_catalog.markdown_jobs (source_id)
    WHERE state IN ('queued','running');

CREATE TABLE git_catalog.markdown_snapshots (
    source_id uuid NOT NULL REFERENCES git_catalog.markdown_approvals,
    digest text NOT NULL CHECK (digest ~ '^[0-9a-f]{64}$'),
    artifact_digest text NOT NULL CHECK (artifact_digest ~ '^[0-9a-f]{64}$'),
    policy_version bigint NOT NULL,
    repository_id uuid,
    generation_id bigint,
    commit_oid bytea,
    git_policy_version bigint,
    byte_count bigint NOT NULL CHECK (byte_count BETWEEN 0 AND 33554432),
    etag text,
    published_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    retired_at timestamptz,
    PRIMARY KEY (source_id, digest),
    CHECK (num_nonnulls(repository_id, generation_id, commit_oid, git_policy_version) IN (0,4))
);

CREATE UNIQUE INDEX markdown_current_hosted ON git_catalog.markdown_snapshots (source_id)
    WHERE repository_id IS NULL AND retired_at IS NULL;

CREATE TABLE git_catalog.markdown_documents (
    source_id uuid NOT NULL,
    digest text NOT NULL,
    path text COLLATE "C" NOT NULL,
    source_path text NOT NULL,
    item text NOT NULL,
    kind text NOT NULL,
    line bigint NOT NULL CHECK (line > 0),
    markdown bytea NOT NULL CHECK (octet_length(markdown) BETWEEN 1 AND 1048576),
    PRIMARY KEY (source_id, digest, path),
    FOREIGN KEY (source_id, digest) REFERENCES git_catalog.markdown_snapshots ON DELETE CASCADE
);

CREATE VIEW git_catalog.readable_markdown_snapshots AS
    SELECT m.source_id, m.digest, m.policy_version, m.repository_id,
           m.generation_id, encode(m.commit_oid, 'hex') AS commit_oid, g.object_format,
           a.policy -> 'input' AS input, a.policy ->> 'notice' AS notice,
           coalesce((a.policy ->> 'search')::boolean, false) AS search,
           m.retired_at IS NULL AND coalesce(g.is_active, true) AS is_current
    FROM git_catalog.markdown_snapshots m
    JOIN git_catalog.markdown_approvals a USING (source_id, policy_version)
    LEFT JOIN git_catalog.exploration_generations g
      ON (g.repository_id, g.generation_id, g.policy_version) =
         (m.repository_id, m.generation_id, m.git_policy_version)
    WHERE NOT a.revoked AND (m.repository_id IS NULL OR g.repository_id IS NOT NULL)
      AND (m.retired_at IS NULL OR m.retired_at > now() - interval '24 hours');

CREATE VIEW git_catalog.readable_markdown_documents AS
    SELECT d.* FROM git_catalog.markdown_documents d
    JOIN git_catalog.readable_markdown_snapshots s USING (source_id, digest);

GRANT SELECT ON git_catalog.exploration_generations, git_catalog.readable_refs,
    git_catalog.readable_generations, git_catalog.reader_lock_keys TO gh_docs_sync;
GRANT SELECT ON git_catalog.readable_markdown_snapshots, git_catalog.readable_markdown_documents TO gh_docs_serve;
GRANT SELECT, INSERT, UPDATE, DELETE ON git_catalog.markdown_jobs TO gh_docs_sync;
GRANT SELECT, INSERT, DELETE, UPDATE (retired_at, etag, artifact_digest)
    ON git_catalog.markdown_snapshots TO gh_docs_sync;
GRANT INSERT ON git_catalog.markdown_documents TO gh_docs_sync;
GRANT SELECT, UPDATE (next_due) ON git_catalog.markdown_approvals TO gh_docs_sync;
GRANT SELECT, INSERT, UPDATE ON git_catalog.markdown_approvals TO gh_docs_migrate;
INSERT INTO git_catalog.migrations (version) VALUES (9);
