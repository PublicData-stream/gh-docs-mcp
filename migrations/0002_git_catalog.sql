CREATE SCHEMA git_catalog AUTHORIZATION gh_docs_migrate;
REVOKE ALL ON SCHEMA git_catalog FROM PUBLIC;

CREATE TABLE git_catalog.migrations (
    version integer PRIMARY KEY,
    applied_at timestamptz NOT NULL DEFAULT transaction_timestamp()
);

CREATE TABLE git_catalog.repositories (
    repository_id uuid PRIMARY KEY CHECK (uuid_extract_version(repository_id) IS NOT DISTINCT FROM 4),
    object_format text NOT NULL CHECK (object_format IN ('sha1', 'sha256')),
    approval_status text NOT NULL DEFAULT 'unapproved'
        CHECK (approval_status IN ('unapproved', 'approved', 'revoked')),
    next_generation bigint NOT NULL DEFAULT 0 CHECK (next_generation >= 0),
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    UNIQUE (repository_id, object_format)
);

CREATE TABLE git_catalog.repository_sources (
    source_id uuid PRIMARY KEY CHECK (uuid_extract_version(source_id) IS NOT DISTINCT FROM 4),
    repository_id uuid NOT NULL REFERENCES git_catalog.repositories,
    canonical_identity text NOT NULL UNIQUE CHECK (octet_length(canonical_identity) BETWEEN 1 AND 2048),
    acquisition_method text NOT NULL CHECK (acquisition_method IN ('synthetic', 'https')),
    policy_version bigint NOT NULL CHECK (policy_version > 0),
    approved_refs bytea[] NOT NULL CHECK (cardinality(approved_refs) BETWEEN 1 AND 50000),
    review_evidence jsonb NOT NULL CHECK (jsonb_typeof(review_evidence) = 'object'),
    reviewed_at timestamptz NOT NULL,
    UNIQUE (repository_id, source_id)
);

CREATE TABLE git_catalog.repository_generations (
    repository_id uuid NOT NULL,
    generation_id bigint NOT NULL CHECK (generation_id >= 0),
    source_id uuid NOT NULL,
    object_format text NOT NULL,
    policy_version bigint NOT NULL CHECK (policy_version > 0),
    provenance jsonb NOT NULL CHECK (jsonb_typeof(provenance) = 'object'),
    state text NOT NULL DEFAULT 'candidate'
        CHECK (state IN ('candidate', 'verified', 'failed', 'discarded')),
    default_ref bytea,
    manifest_digest bytea CHECK (octet_length(manifest_digest) = 32),
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    observed_at timestamptz NOT NULL,
    finalized_at timestamptz,
    CHECK ((state = 'candidate') = (finalized_at IS NULL)),
    CHECK ((state = 'verified') = (manifest_digest IS NOT NULL)),
    PRIMARY KEY (repository_id, generation_id),
    UNIQUE (repository_id, generation_id, object_format),
    FOREIGN KEY (repository_id, object_format) REFERENCES git_catalog.repositories
        (repository_id, object_format),
    FOREIGN KEY (repository_id, source_id) REFERENCES git_catalog.repository_sources
        (repository_id, source_id)
);

CREATE TABLE git_catalog.generation_refs (
    repository_id uuid NOT NULL,
    generation_id bigint NOT NULL,
    object_format text NOT NULL,
    ref_name bytea NOT NULL CHECK (
        octet_length(ref_name) BETWEEN 6 AND 4096
        AND substring(ref_name FROM 1 FOR 5) = '\x726566732f'::bytea
        AND position('\x00'::bytea IN ref_name) = 0
    ),
    target_oid bytea NOT NULL,
    target_type text NOT NULL CHECK (target_type IN ('blob', 'tree', 'commit', 'tag')),
    peeled_commit bytea,
    CHECK (octet_length(target_oid) = CASE object_format WHEN 'sha1' THEN 20 WHEN 'sha256' THEN 32 END),
    CHECK (peeled_commit IS NULL OR (
        octet_length(peeled_commit) = octet_length(target_oid) AND target_type IN ('tag', 'commit')
    )),
    CHECK (target_type <> 'commit' OR (peeled_commit IS NOT NULL AND peeled_commit = target_oid)),
    PRIMARY KEY (repository_id, generation_id, ref_name),
    FOREIGN KEY (repository_id, generation_id, object_format)
        REFERENCES git_catalog.repository_generations (repository_id, generation_id, object_format)
);

ALTER TABLE git_catalog.repository_generations ADD FOREIGN KEY
    (repository_id, generation_id, default_ref) REFERENCES git_catalog.generation_refs
    DEFERRABLE INITIALLY DEFERRED;

CREATE TABLE git_catalog.active_generations (
    repository_id uuid PRIMARY KEY,
    generation_id bigint NOT NULL,
    publication_sequence bigint NOT NULL CHECK (publication_sequence >= 0),
    published_at timestamptz NOT NULL,
    FOREIGN KEY (repository_id, generation_id) REFERENCES git_catalog.repository_generations
);

CREATE TABLE git_catalog.sync_jobs (
    job_id uuid PRIMARY KEY DEFAULT uuidv7() CHECK (uuid_extract_version(job_id) IS NOT DISTINCT FROM 7),
    repository_id uuid NOT NULL REFERENCES git_catalog.repositories,
    expected_publication_sequence bigint NOT NULL CHECK (expected_publication_sequence >= 0),
    state text NOT NULL CHECK (state IN ('queued', 'running', 'succeeded', 'failed')),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    worker_id uuid,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp()
);

CREATE TABLE git_catalog.notification_outbox (
    event_id uuid PRIMARY KEY DEFAULT uuidv7() CHECK (uuid_extract_version(event_id) IS NOT DISTINCT FROM 7),
    repository_id uuid NOT NULL,
    generation_id bigint NOT NULL,
    publication_sequence bigint NOT NULL CHECK (publication_sequence >= 0),
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    delivered_at timestamptz,
    UNIQUE (repository_id, publication_sequence),
    FOREIGN KEY (repository_id, generation_id) REFERENCES git_catalog.repository_generations
);

CREATE TABLE git_catalog.repository_incidents (
    incident_id uuid PRIMARY KEY DEFAULT uuidv7() CHECK (uuid_extract_version(incident_id) IS NOT DISTINCT FROM 7),
    repository_id uuid NOT NULL REFERENCES git_catalog.repositories,
    generation_id bigint,
    reason_code text NOT NULL CHECK (reason_code ~ '^[a-z][a-z0-9_]{0,63}$'),
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    resolved_at timestamptz,
    FOREIGN KEY (repository_id, generation_id) REFERENCES git_catalog.repository_generations
);

CREATE TABLE git_catalog.lock_keys (
    advisory_key bigint GENERATED ALWAYS AS IDENTITY (START WITH 1024) PRIMARY KEY,
    scope text NOT NULL CHECK (scope IN ('coordinator', 'repository', 'generation')),
    repository_id uuid NOT NULL REFERENCES git_catalog.repositories,
    generation_id bigint,
    CHECK ((scope = 'generation') = (generation_id IS NOT NULL)),
    UNIQUE NULLS NOT DISTINCT (scope, repository_id, generation_id),
    FOREIGN KEY (repository_id, generation_id) REFERENCES git_catalog.repository_generations
);

CREATE VIEW git_catalog.visible_repositories AS
    SELECT r.repository_id, r.object_format, a.generation_id, a.publication_sequence
    FROM git_catalog.repositories r
    JOIN git_catalog.active_generations a USING (repository_id)
    JOIN git_catalog.repository_generations g USING (repository_id, generation_id)
    WHERE r.approval_status = 'approved' AND g.state = 'active';

CREATE FUNCTION git_catalog.valid_ref(ref bytea)
RETURNS boolean LANGUAGE sql IMMUTABLE STRICT SET search_path = pg_catalog AS $$
    SELECT octet_length(ref) BETWEEN 6 AND 4096
        AND substring(ref FROM 1 FOR 5) = '\x726566732f'::bytea
        AND NOT EXISTS (
            SELECT 1 FROM generate_series(0, octet_length(ref)-1) AS offsets(i)
            WHERE get_byte(ref, i) < 33 OR get_byte(ref, i) IN (42, 58, 63, 91, 92, 94, 126, 127)
        )
        AND convert_from(ref, 'LATIN1') COLLATE "C" !~ '(^|/)\.|//|/$|\.lock(/|$)|\.$|\.\.|@\{'
$$;
REVOKE EXECUTE ON FUNCTION git_catalog.valid_ref(bytea) FROM PUBLIC;

CREATE FUNCTION git_catalog.register_fixture(
    repo uuid, source uuid, format text, identity text, refs bytea[]
) RETURNS void LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
BEGIN
    IF identity IS DISTINCT FROM 'urn:gh-docs:fixture:' || repo::text
        OR cardinality(refs) NOT BETWEEN 1 AND 50000
        OR array_ndims(refs) IS DISTINCT FROM 1
        OR EXISTS (SELECT 1 FROM unnest(refs) AS names(ref)
                   WHERE git_catalog.valid_ref(ref) IS DISTINCT FROM true)
        OR (SELECT count(DISTINCT ref) FROM unnest(refs) AS names(ref)) <> cardinality(refs)
    THEN
        RAISE EXCEPTION 'invalid synthetic fixture scope' USING ERRCODE = '22023';
    END IF;
    INSERT INTO git_catalog.repositories (repository_id, object_format) VALUES (repo, format)
        ON CONFLICT (repository_id) DO NOTHING;
    -- Serialize retries and ensure a reused UUID still has the same fixture identity.
    PERFORM 1 FROM git_catalog.repositories
        WHERE repository_id = repo AND object_format = format AND approval_status = 'unapproved'
        FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'fixture repository identity mismatch' USING ERRCODE = '22023';
    END IF;
    INSERT INTO git_catalog.repository_sources
        (source_id, repository_id, canonical_identity, acquisition_method, policy_version,
         approved_refs, review_evidence, reviewed_at)
    VALUES (source, repo, identity, 'synthetic', 1, refs,
        '{"reviewer":"phase1-fixture","scope":"offline synthetic qualification"}'::jsonb,
        transaction_timestamp())
    ON CONFLICT (source_id) DO NOTHING;
    PERFORM 1 FROM git_catalog.repository_sources
        WHERE source_id = source AND repository_id = repo AND canonical_identity = identity
            AND acquisition_method = 'synthetic' AND policy_version = 1 AND approved_refs = refs;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'fixture source identity mismatch' USING ERRCODE = '22023';
    END IF;
    INSERT INTO git_catalog.lock_keys (scope, repository_id)
        VALUES ('coordinator', repo), ('repository', repo) ON CONFLICT DO NOTHING;
END
$$;
REVOKE EXECUTE ON FUNCTION git_catalog.register_fixture(uuid, uuid, text, text, bytea[]) FROM PUBLIC;

CREATE FUNCTION git_catalog.begin_fixture_candidate(repo uuid, source uuid, default_name bytea, snapshot jsonb)
RETURNS bigint LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
DECLARE
    generation bigint;
    format text;
    registered git_catalog.repository_sources%ROWTYPE;
BEGIN
    UPDATE git_catalog.repositories SET next_generation = next_generation + 1
        WHERE repository_id = repo AND approval_status = 'unapproved'
        RETURNING next_generation - 1, object_format INTO STRICT generation, format;
    SELECT * INTO STRICT registered FROM git_catalog.repository_sources
        WHERE source_id = source AND repository_id = repo AND acquisition_method = 'synthetic';
    IF jsonb_array_length(snapshot) NOT BETWEEN 1 AND cardinality(registered.approved_refs) THEN
        RAISE EXCEPTION 'invalid fixture snapshot size' USING ERRCODE = '22023';
    END IF;
    INSERT INTO git_catalog.repository_generations
        (repository_id, generation_id, source_id, object_format, policy_version, provenance,
         default_ref, observed_at)
    VALUES (repo, generation, source, format, registered.policy_version,
        jsonb_build_object('identity', registered.canonical_identity, 'acquisition', registered.acquisition_method,
            'ref_scope_hex', (SELECT jsonb_agg(encode(ref, 'hex') ORDER BY ref)
                FROM unnest(registered.approved_refs) AS names(ref)),
            'review', registered.review_evidence, 'reviewed_at', registered.reviewed_at),
        default_name, transaction_timestamp());
    INSERT INTO git_catalog.generation_refs
        (repository_id, generation_id, object_format, ref_name, target_oid, target_type, peeled_commit)
    SELECT repo, generation, format, decode(entry.ref, 'hex'), decode(entry.target, 'hex'),
        entry.kind, decode(entry.peeled, 'hex')
    FROM jsonb_to_recordset(snapshot) AS entry(ref text, target text, kind text, peeled text);
    IF EXISTS (SELECT 1 FROM git_catalog.generation_refs r
        WHERE r.repository_id = repo AND r.generation_id = generation
            AND NOT (r.ref_name = ANY (registered.approved_refs))) THEN
        RAISE EXCEPTION 'fixture snapshot exceeds approved ref scope' USING ERRCODE = '22023';
    END IF;
    INSERT INTO git_catalog.lock_keys (scope, repository_id, generation_id)
        VALUES ('generation', repo, generation);
    RETURN generation;
END
$$;
REVOKE EXECUTE ON FUNCTION git_catalog.begin_fixture_candidate(uuid, uuid, bytea, jsonb) FROM PUBLIC;

CREATE FUNCTION git_catalog.finish_fixture_candidate(repo uuid, generation bigint, result text, digest bytea)
RETURNS void LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
BEGIN
    IF result IS NULL OR result NOT IN ('verified', 'failed', 'discarded') THEN
        RAISE EXCEPTION 'invalid candidate disposition' USING ERRCODE = '22023';
    END IF;
    UPDATE git_catalog.repository_generations g
        SET state = result, manifest_digest = digest, finalized_at = transaction_timestamp()
        WHERE g.repository_id = repo AND g.generation_id = generation AND state = 'candidate'
            AND EXISTS (SELECT 1 FROM git_catalog.repository_sources s
                        WHERE s.source_id = g.source_id AND s.acquisition_method = 'synthetic');
    IF NOT FOUND AND NOT EXISTS (SELECT 1 FROM git_catalog.repository_generations g
        JOIN git_catalog.repository_sources s USING (source_id)
        WHERE g.repository_id = repo AND g.generation_id = generation
            AND s.acquisition_method = 'synthetic' AND g.state = result
            AND g.manifest_digest IS NOT DISTINCT FROM digest) THEN
        RAISE EXCEPTION 'fixture candidate cannot transition' USING ERRCODE = '22023';
    END IF;
END
$$;
REVOKE EXECUTE ON FUNCTION git_catalog.finish_fixture_candidate(uuid, bigint, text, bytea) FROM PUBLIC;

GRANT USAGE ON SCHEMA git_catalog TO gh_docs_serve, gh_docs_sync;
GRANT SELECT ON git_catalog.visible_repositories TO gh_docs_serve;
GRANT SELECT ON git_catalog.repositories, git_catalog.repository_sources,
    git_catalog.repository_generations, git_catalog.generation_refs, git_catalog.lock_keys TO gh_docs_sync;
GRANT EXECUTE ON FUNCTION git_catalog.register_fixture(uuid, uuid, text, text, bytea[]),
    git_catalog.begin_fixture_candidate(uuid, uuid, bytea, jsonb),
    git_catalog.finish_fixture_candidate(uuid, bigint, text, bytea)
    TO gh_docs_sync;

INSERT INTO git_catalog.migrations (version) VALUES (2);
