ALTER TABLE git_catalog.repository_sources DROP CONSTRAINT repository_sources_approved_refs_check;
ALTER TABLE git_catalog.repository_sources ADD CHECK (
    cardinality(approved_refs) BETWEEN 1 AND 50000 OR
    (cardinality(approved_refs) = 0 AND acquisition_method = 'https'));

CREATE TABLE git_catalog.source_approvals (
    source_id uuid NOT NULL REFERENCES git_catalog.repository_sources,
    policy_version bigint NOT NULL CHECK (policy_version > 0),
    approval jsonb NOT NULL CHECK (jsonb_typeof(approval) = 'object'),
    recorded_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (source_id, policy_version)
);

CREATE TABLE git_catalog.private_candidates (
    repository_id uuid NOT NULL,
    generation_id bigint NOT NULL,
    job_id uuid NOT NULL UNIQUE REFERENCES git_catalog.sync_jobs,
    logical_digest bytea NOT NULL CHECK (octet_length(logical_digest) = 32),
    pack_digest bytea NOT NULL CHECK (octet_length(pack_digest) = 32),
    state text NOT NULL DEFAULT 'usable' CHECK (state IN ('usable', 'retired', 'corrupt')),
    PRIMARY KEY (repository_id, generation_id),
    FOREIGN KEY (repository_id, generation_id) REFERENCES git_catalog.repository_generations
);

ALTER TABLE git_catalog.sync_jobs ADD COLUMN source_id uuid;
ALTER TABLE git_catalog.sync_jobs ADD COLUMN policy_version bigint;
ALTER TABLE git_catalog.sync_jobs ADD COLUMN outcome text;
ALTER TABLE git_catalog.sync_jobs ADD COLUMN observation jsonb;
ALTER TABLE git_catalog.sync_jobs ADD COLUMN finished_at timestamptz;
ALTER TABLE git_catalog.sync_jobs ADD COLUMN cleaned_at timestamptz;
ALTER TABLE git_catalog.sync_jobs ADD FOREIGN KEY (repository_id, source_id)
    REFERENCES git_catalog.repository_sources (repository_id, source_id);
ALTER TABLE git_catalog.sync_jobs ADD FOREIGN KEY (source_id, policy_version)
    REFERENCES git_catalog.source_approvals;
CREATE INDEX sync_jobs_private_cleanup ON git_catalog.sync_jobs (repository_id, finished_at, job_id)
    WHERE source_id IS NOT NULL AND cleaned_at IS NULL;

CREATE TABLE git_catalog.upstream_pacing (
    host text PRIMARY KEY,
    last_started timestamptz NOT NULL
);

CREATE FUNCTION git_catalog.approve_source(repo uuid, source uuid, format text, url text,
    version bigint, exact_refs bytea[], approval jsonb)
RETURNS void LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
BEGIN
    INSERT INTO git_catalog.repositories (repository_id, object_format)
        VALUES (repo, format) ON CONFLICT (repository_id) DO NOTHING;
    PERFORM 1 FROM git_catalog.repositories WHERE repository_id = repo
        AND object_format = format FOR UPDATE;
    IF NOT FOUND THEN RAISE EXCEPTION 'repository format conflict'; END IF;
    IF EXISTS (SELECT 1 FROM git_catalog.repository_sources
        WHERE repository_id = repo AND acquisition_method = 'https' AND source_id <> source)
    THEN RAISE EXCEPTION 'repository already has a canonical HTTPS source'; END IF;
    INSERT INTO git_catalog.repository_sources
        (source_id, repository_id, canonical_identity, acquisition_method, policy_version,
         approved_refs, review_evidence, reviewed_at)
        VALUES (source, repo, url, 'https', version, exact_refs, approval,
                (approval->>'reviewed_at')::timestamptz)
    ON CONFLICT (source_id) DO UPDATE SET policy_version = excluded.policy_version,
        approved_refs = excluded.approved_refs, review_evidence = excluded.review_evidence,
        reviewed_at = excluded.reviewed_at
    WHERE repository_sources.repository_id = repo AND repository_sources.canonical_identity = url
        AND repository_sources.acquisition_method = 'https'
        AND repository_sources.policy_version < version;
    IF NOT FOUND THEN RAISE EXCEPTION 'source identity or policy version conflict'; END IF;
    INSERT INTO git_catalog.source_approvals VALUES (source, version, approval);
    UPDATE git_catalog.repositories SET approval_status = 'approved' WHERE repository_id = repo;
    INSERT INTO git_catalog.lock_keys (scope, repository_id) VALUES ('coordinator', repo), ('repository', repo)
        ON CONFLICT DO NOTHING;
END;
$$;

CREATE FUNCTION git_catalog.revoke_source(source uuid)
RETURNS void LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
BEGIN
    UPDATE git_catalog.repositories r SET approval_status = 'revoked'
        FROM git_catalog.repository_sources s
        WHERE s.source_id = source AND s.acquisition_method = 'https' AND r.repository_id = s.repository_id;
    IF NOT FOUND THEN RAISE EXCEPTION 'unknown HTTPS source'; END IF;
END;
$$;

CREATE FUNCTION git_catalog.start_reconcile(repo uuid, source uuid, version bigint)
RETURNS uuid LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
DECLARE job uuid;
BEGIN
    PERFORM 1 FROM git_catalog.repositories r JOIN git_catalog.repository_sources s USING (repository_id)
        WHERE r.repository_id = repo AND r.approval_status = 'approved'
            AND s.source_id = source AND s.policy_version = version AND s.acquisition_method = 'https'
        FOR UPDATE OF r;
    IF NOT FOUND THEN RAISE EXCEPTION 'source approval changed'; END IF;
    WITH interrupted AS (
        UPDATE git_catalog.sync_jobs SET state = 'failed', outcome = 'interrupted', finished_at = clock_timestamp()
        WHERE repository_id = repo AND source_id IS NOT NULL AND state = 'running' RETURNING repository_id
    )
    INSERT INTO git_catalog.repository_incidents (repository_id, reason_code)
        SELECT repository_id, 'interrupted' FROM interrupted;
    INSERT INTO git_catalog.sync_jobs
        (repository_id, source_id, policy_version, expected_publication_sequence, state, attempts)
        VALUES (repo, source, version, coalesce((SELECT publication_sequence
            FROM git_catalog.active_generations WHERE repository_id = repo), 0), 'running', 1)
        RETURNING job_id INTO job;
    RETURN job;
END;
$$;

CREATE FUNCTION git_catalog.acquire_upstream_start(job uuid, host_name text)
RETURNS boolean LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
BEGIN
    PERFORM 1 FROM git_catalog.sync_jobs j JOIN git_catalog.repositories r USING (repository_id)
        JOIN git_catalog.repository_sources s ON s.source_id = j.source_id
        JOIN git_catalog.source_approvals a ON a.source_id = s.source_id AND a.policy_version = s.policy_version
        WHERE j.job_id = job AND j.state = 'running' AND r.approval_status = 'approved'
            AND s.policy_version = j.policy_version AND a.approval->'hosts' ? host_name
        FOR SHARE OF r;
    IF NOT FOUND THEN RAISE EXCEPTION 'source approval changed'; END IF;
    INSERT INTO git_catalog.upstream_pacing VALUES (host_name, clock_timestamp())
    ON CONFLICT (host) DO UPDATE SET last_started = clock_timestamp()
        WHERE upstream_pacing.last_started <= clock_timestamp() - interval '1 second';
    RETURN FOUND;
END;
$$;

CREATE FUNCTION git_catalog.fail_reconcile(job uuid, reason text, attempt_count integer)
RETURNS boolean LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
DECLARE repo uuid;
BEGIN
    UPDATE git_catalog.sync_jobs SET state = 'failed', outcome = reason, attempts = attempt_count,
        finished_at = clock_timestamp()
        WHERE job_id = job AND source_id IS NOT NULL AND state = 'running' RETURNING repository_id INTO repo;
    IF FOUND THEN
        INSERT INTO git_catalog.repository_incidents (repository_id, reason_code) VALUES (repo, reason);
        RETURN true;
    END IF;
    RETURN false;
END;
$$;

CREATE FUNCTION git_catalog.confirm_private_cleanup(job uuid)
RETURNS void LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
    UPDATE git_catalog.sync_jobs SET cleaned_at = clock_timestamp()
        WHERE job_id = job AND source_id IS NOT NULL AND state IN ('failed', 'succeeded')
          AND cleaned_at IS NULL AND NOT EXISTS (
              SELECT 1 FROM git_catalog.private_candidates WHERE job_id = job AND state = 'usable')
$$;

CREATE FUNCTION git_catalog.flag_corrupt_candidate(repo uuid, generation bigint)
RETURNS void LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
BEGIN
    UPDATE git_catalog.private_candidates SET state = 'corrupt'
        WHERE repository_id = repo AND generation_id = generation AND state = 'usable';
    IF FOUND THEN
        INSERT INTO git_catalog.repository_incidents (repository_id, generation_id, reason_code)
            VALUES (repo, generation, 'baseline_corrupt');
    END IF;
END;
$$;

CREATE FUNCTION git_catalog.seal_candidate(job uuid, logical bytea, observed bytea, pack bytea,
    default_name bytea, snapshot jsonb, proof jsonb, observed_time text, attempt_count integer)
RETURNS bigint LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
DECLARE
    running git_catalog.sync_jobs%ROWTYPE;
    format text;
    generation bigint;
BEGIN
    SELECT * INTO STRICT running FROM git_catalog.sync_jobs WHERE job_id = job
        AND source_id IS NOT NULL AND state = 'running' FOR UPDATE;
    SELECT r.object_format INTO STRICT format
        FROM git_catalog.repositories r JOIN git_catalog.repository_sources s USING (repository_id)
        WHERE r.repository_id = running.repository_id AND r.approval_status = 'approved'
            AND s.source_id = running.source_id AND s.policy_version = running.policy_version
        FOR UPDATE OF r;

    SELECT p.generation_id INTO generation
        FROM git_catalog.private_candidates p JOIN git_catalog.repository_generations g
            USING (repository_id, generation_id)
        WHERE p.repository_id = running.repository_id AND p.state = 'usable'
          AND g.source_id = running.source_id AND g.policy_version = running.policy_version
        ORDER BY p.generation_id DESC LIMIT 1;
    IF proof->>'baseline_generation' = generation::text
        AND EXISTS (SELECT 1 FROM git_catalog.private_candidates WHERE repository_id = running.repository_id
        AND generation_id = generation AND logical_digest = logical)
    THEN
        UPDATE git_catalog.sync_jobs SET state = 'succeeded', outcome = 'no_change',
            attempts = attempt_count, observation = proof || jsonb_build_object('manifest_digest', encode(observed, 'hex')),
            finished_at = clock_timestamp() WHERE job_id = job;
        RETURN NULL;
    END IF;

    IF jsonb_array_length(snapshot) NOT BETWEEN 1 AND 1024 THEN RAISE EXCEPTION 'invalid snapshot size'; END IF;
    UPDATE git_catalog.repositories SET next_generation = next_generation + 1
        WHERE repository_id = running.repository_id RETURNING next_generation - 1 INTO generation;
    INSERT INTO git_catalog.repository_generations
        (repository_id, generation_id, source_id, object_format, policy_version, provenance,
         state, default_ref, manifest_digest, observed_at, finalized_at)
        VALUES (running.repository_id, generation, running.source_id, format, running.policy_version,
                proof, 'verified', default_name, observed, observed_time::timestamptz, clock_timestamp());
    INSERT INTO git_catalog.generation_refs
        SELECT running.repository_id, generation, format, decode(ref->>0, 'base64'),
            decode(ref->>2, 'hex'), ref->>1, decode(ref->>3, 'hex')
        FROM jsonb_array_elements(snapshot) AS ref WHERE git_catalog.valid_ref(decode(ref->>0, 'base64'));
    IF (SELECT count(*) FROM git_catalog.generation_refs WHERE repository_id = running.repository_id
        AND generation_id = generation) <> jsonb_array_length(snapshot)
    THEN RAISE EXCEPTION 'invalid reference snapshot'; END IF;
    INSERT INTO git_catalog.private_candidates
        (repository_id, generation_id, job_id, logical_digest, pack_digest)
        VALUES (running.repository_id, generation, job, logical, pack);
    INSERT INTO git_catalog.lock_keys (scope, repository_id, generation_id)
        VALUES ('generation', running.repository_id, generation);
    UPDATE git_catalog.private_candidates SET state = 'retired' WHERE repository_id = running.repository_id
        AND state = 'usable' AND generation_id IN (SELECT p.generation_id FROM git_catalog.private_candidates p
            WHERE p.repository_id = running.repository_id AND p.state = 'usable'
            ORDER BY p.generation_id DESC OFFSET 2);
    UPDATE git_catalog.sync_jobs SET state = 'succeeded', outcome = 'verified',
        attempts = attempt_count, observation = proof, finished_at = clock_timestamp() WHERE job_id = job;
    RETURN generation;
END;
$$;

REVOKE EXECUTE ON ALL FUNCTIONS IN SCHEMA git_catalog FROM PUBLIC;
GRANT SELECT ON git_catalog.private_candidates, git_catalog.sync_jobs TO gh_docs_sync;
GRANT EXECUTE ON FUNCTION git_catalog.start_reconcile(uuid, uuid, bigint),
    git_catalog.acquire_upstream_start(uuid, text), git_catalog.fail_reconcile(uuid, text, integer),
    git_catalog.confirm_private_cleanup(uuid),
    git_catalog.flag_corrupt_candidate(uuid, bigint),
    git_catalog.seal_candidate(uuid, bytea, bytea, bytea, bytea, jsonb, jsonb, text, integer)
    TO gh_docs_sync;
INSERT INTO git_catalog.migrations (version) VALUES (3);
