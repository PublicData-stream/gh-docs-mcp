ALTER TABLE git_catalog.repository_generations
    DROP CONSTRAINT repository_generations_state_check,
    DROP CONSTRAINT repository_generations_check1,
    ADD COLUMN demoted_at timestamptz,
    ADD COLUMN last_publication_sequence bigint,
    ADD COLUMN retired_at timestamptz,
    ADD CHECK (state IN ('candidate', 'verified', 'failed', 'discarded', 'ready', 'active', 'retained', 'retired')),
    ADD CHECK ((state IN ('verified', 'ready', 'active', 'retained', 'retired')) = (manifest_digest IS NOT NULL)
        OR (state = 'discarded' AND manifest_digest IS NOT NULL)),
    ADD CHECK ((state = 'retained') = (demoted_at IS NOT NULL)),
    ADD CHECK ((state IN ('active', 'retained', 'retired')) = (last_publication_sequence IS NOT NULL)),
    ADD CHECK (last_publication_sequence > 0),
    ADD CHECK ((state = 'retired') = (retired_at IS NOT NULL));

CREATE UNIQUE INDEX one_active_generation ON git_catalog.repository_generations (repository_id)
    WHERE state = 'active';

CREATE TABLE git_catalog.publication_blocks (
    repository_id uuid PRIMARY KEY REFERENCES git_catalog.repositories,
    reason_code text NOT NULL CHECK (reason_code ~ '^[a-z][a-z0-9_]{0,63}$')
);

CREATE TABLE git_catalog.publication_work (
    repository_id uuid PRIMARY KEY REFERENCES git_catalog.repositories,
    generation_id bigint,
    action text NOT NULL CHECK (action IN ('prepare', 'retire', 'discard', 'repack')),
    operation_id uuid NOT NULL CHECK (uuid_extract_version(operation_id) IS NOT DISTINCT FROM 7),
    details jsonb NOT NULL CHECK (jsonb_typeof(details) = 'object'),
    CHECK ((action = 'repack') = (generation_id IS NULL)),
    FOREIGN KEY (repository_id, generation_id) REFERENCES git_catalog.repository_generations
);

CREATE TABLE git_catalog.publication_packs (
    repository_id uuid NOT NULL REFERENCES git_catalog.repositories,
    filename text NOT NULL CHECK (filename ~ '^pack-([a-f0-9]{40}|[a-f0-9]{64})\.(pack|idx|mtimes)$'),
    checksum text NOT NULL CHECK (checksum ~ '^[a-f0-9]{64}$'),
    PRIMARY KEY (repository_id, filename)
);

CREATE VIEW git_catalog.readable_generations AS
    SELECT g.repository_id, g.generation_id, g.object_format, g.source_id, g.policy_version,
        g.default_ref, g.manifest_digest, g.last_publication_sequence AS publication_sequence,
        (a.generation_id = g.generation_id) AS is_active
    FROM git_catalog.repository_generations g
    JOIN git_catalog.repositories r USING (repository_id)
    JOIN git_catalog.repository_sources s ON (s.repository_id, s.source_id) = (g.repository_id, g.source_id)
    JOIN git_catalog.source_approvals p ON (p.source_id, p.policy_version) = (g.source_id, g.policy_version)
    JOIN git_catalog.active_generations a ON a.repository_id = g.repository_id
    LEFT JOIN git_catalog.publication_blocks b ON b.repository_id = g.repository_id
    WHERE r.approval_status = 'approved' AND s.policy_version = g.policy_version
        AND b.repository_id IS NULL
        AND ((g.state = 'active' AND a.generation_id = g.generation_id
              AND a.publication_sequence = g.last_publication_sequence)
          OR (g.state = 'retained' AND a.generation_id <> g.generation_id));

CREATE OR REPLACE VIEW git_catalog.visible_repositories AS
    SELECT repository_id, object_format, generation_id, publication_sequence
    FROM git_catalog.readable_generations WHERE is_active;

CREATE VIEW git_catalog.readable_refs AS
    SELECT f.* FROM git_catalog.generation_refs f
    JOIN git_catalog.readable_generations USING (repository_id, generation_id);

CREATE VIEW git_catalog.reader_lock_keys AS
    SELECT k.advisory_key, k.repository_id, k.generation_id
    FROM git_catalog.lock_keys k WHERE
        (k.scope = 'repository' AND EXISTS (SELECT 1 FROM git_catalog.readable_generations g
                                           WHERE g.repository_id = k.repository_id))
        OR (k.scope = 'generation' AND EXISTS (SELECT 1 FROM git_catalog.readable_generations g
                           WHERE (g.repository_id, g.generation_id) = (k.repository_id, k.generation_id)));

CREATE FUNCTION git_catalog.require_publication_fence(repo uuid, lock_scope text, generation bigint)
RETURNS void LANGUAGE plpgsql SET search_path = pg_catalog AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM git_catalog.lock_keys k JOIN pg_locks l
            ON l.classid::bigint = (k.advisory_key >> 32)
           AND l.objid::bigint = (k.advisory_key & 4294967295)
        WHERE k.repository_id = repo AND k.scope = lock_scope
          AND k.generation_id IS NOT DISTINCT FROM generation
          AND l.pid = pg_backend_pid() AND l.locktype = 'advisory'
          AND l.objsubid = 1 AND l.granted AND l.mode = 'ExclusiveLock'
    ) THEN RAISE EXCEPTION 'publication fence required' USING ERRCODE = '55000'; END IF;
END;
$$;

ALTER FUNCTION git_catalog.approve_source(uuid, uuid, text, text, bigint, bytea[], jsonb)
    RENAME TO approve_source_unfenced;
CREATE FUNCTION git_catalog.approve_source(repo uuid, source uuid, format text, url text,
    version bigint, exact_refs bytea[], approval jsonb)
RETURNS void LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
DECLARE lock_key bigint;
BEGIN
    INSERT INTO git_catalog.repositories (repository_id, object_format)
        VALUES (repo, format) ON CONFLICT (repository_id) DO NOTHING;
    INSERT INTO git_catalog.lock_keys (scope, repository_id) VALUES ('coordinator', repo), ('repository', repo)
        ON CONFLICT DO NOTHING;
    SELECT advisory_key INTO STRICT lock_key FROM git_catalog.lock_keys
        WHERE repository_id = repo AND scope = 'repository';
    PERFORM pg_advisory_xact_lock(lock_key);
    PERFORM git_catalog.approve_source_unfenced(repo, source, format, url, version, exact_refs, approval);
END;
$$;

ALTER FUNCTION git_catalog.revoke_source(uuid) RENAME TO revoke_source_unfenced;
CREATE FUNCTION git_catalog.revoke_source(source uuid)
RETURNS void LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
DECLARE lock_key bigint;
BEGIN
    SELECT k.advisory_key INTO STRICT lock_key
        FROM git_catalog.repository_sources s JOIN git_catalog.lock_keys k USING (repository_id)
        WHERE s.source_id = source AND s.acquisition_method = 'https' AND k.scope = 'repository';
    PERFORM pg_advisory_xact_lock(lock_key);
    PERFORM git_catalog.revoke_source_unfenced(source);
END;
$$;

CREATE FUNCTION git_catalog.current_generation_approval(repo uuid, generation bigint)
RETURNS boolean LANGUAGE sql STABLE SET search_path = pg_catalog AS $$
    SELECT EXISTS (
        SELECT 1 FROM git_catalog.repository_generations g
        JOIN git_catalog.repositories r USING (repository_id)
        JOIN git_catalog.repository_sources s ON (s.repository_id, s.source_id) = (g.repository_id, g.source_id)
        JOIN git_catalog.source_approvals p ON (p.source_id, p.policy_version) = (g.source_id, g.policy_version)
        WHERE g.repository_id = repo AND g.generation_id = generation AND r.approval_status = 'approved'
          AND g.policy_version = s.policy_version AND s.acquisition_method = 'https')
$$;

CREATE FUNCTION git_catalog.publication_incident(repo uuid, generation bigint, reason text, blocked boolean)
RETURNS void LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
BEGIN
    PERFORM git_catalog.require_publication_fence(repo, 'coordinator', NULL);
    PERFORM git_catalog.require_publication_fence(repo, 'repository', NULL);
    IF NOT EXISTS (SELECT 1 FROM git_catalog.repository_incidents i WHERE i.repository_id = repo
        AND i.generation_id IS NOT DISTINCT FROM generation AND i.reason_code = reason AND i.resolved_at IS NULL)
    THEN
        INSERT INTO git_catalog.repository_incidents (repository_id, generation_id, reason_code)
            VALUES (repo, generation, reason);
    END IF;
    IF blocked THEN
        INSERT INTO git_catalog.publication_blocks VALUES (repo, reason)
            ON CONFLICT (repository_id) DO UPDATE SET reason_code = excluded.reason_code;
    END IF;
END;
$$;

CREATE FUNCTION git_catalog.begin_publication_work(repo uuid, generation bigint, kind text, details jsonb)
RETURNS uuid LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
DECLARE operation uuid := uuidv7();
DECLARE lifecycle text;
BEGIN
    PERFORM git_catalog.require_publication_fence(repo, 'coordinator', NULL);
    PERFORM git_catalog.require_publication_fence(repo, 'repository', NULL);
    IF generation IS NOT NULL THEN
        PERFORM git_catalog.require_publication_fence(repo, 'generation', generation);
        SELECT g.state INTO STRICT lifecycle FROM git_catalog.repository_generations g
            WHERE g.repository_id = repo AND g.generation_id = generation;
    END IF;
    IF kind = 'prepare' THEN
        IF lifecycle IS DISTINCT FROM 'verified' OR NOT git_catalog.current_generation_approval(repo, generation)
            OR NOT EXISTS (SELECT 1 FROM git_catalog.private_candidates p WHERE p.repository_id = repo
                AND p.generation_id = generation AND p.state = 'usable')
            OR EXISTS (SELECT 1 FROM git_catalog.publication_blocks WHERE repository_id = repo)
        THEN RAISE EXCEPTION 'verified candidate unavailable'; END IF;
    ELSIF kind = 'retire' THEN
        UPDATE git_catalog.repository_generations g
            SET state = 'retired', retired_at = clock_timestamp(), demoted_at = NULL
            WHERE g.repository_id = repo AND g.generation_id = generation AND g.state = 'retained'
              AND g.demoted_at <= clock_timestamp() - interval '24 hours'
              AND g.generation_id NOT IN (SELECT recent.generation_id FROM git_catalog.repository_generations recent
                  WHERE recent.repository_id = repo AND recent.state = 'retained'
                  ORDER BY recent.last_publication_sequence DESC LIMIT 3);
        IF NOT FOUND THEN RAISE EXCEPTION 'generation is protected from retirement'; END IF;
    ELSIF kind = 'discard' THEN
        UPDATE git_catalog.repository_generations g SET state = 'discarded'
            WHERE g.repository_id = repo AND g.generation_id = generation AND g.state IN ('verified', 'ready')
                AND EXISTS (SELECT 1 FROM git_catalog.private_candidates p
                    WHERE p.repository_id = repo AND p.generation_id = generation);
        IF NOT FOUND THEN RAISE EXCEPTION 'candidate cannot be discarded'; END IF;
        UPDATE git_catalog.private_candidates p SET state = 'retired'
            WHERE p.repository_id = repo AND p.generation_id = generation AND p.state = 'usable';
    ELSIF kind = 'repack' THEN
        INSERT INTO git_catalog.publication_blocks VALUES (repo, 'repack_pending')
            ON CONFLICT (repository_id) DO UPDATE SET reason_code = excluded.reason_code;
    ELSE RAISE EXCEPTION 'invalid publication operation';
    END IF;
    INSERT INTO git_catalog.publication_work
        VALUES (repo, generation, kind, operation, details);
    RETURN operation;
END;
$$;

CREATE FUNCTION git_catalog.finish_publication_work(repo uuid, operation uuid, complete boolean)
RETURNS void LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
DECLARE work git_catalog.publication_work%ROWTYPE;
BEGIN
    PERFORM git_catalog.require_publication_fence(repo, 'coordinator', NULL);
    PERFORM git_catalog.require_publication_fence(repo, 'repository', NULL);
    SELECT * INTO STRICT work FROM git_catalog.publication_work w
        WHERE w.repository_id = repo AND w.operation_id = operation;
    IF work.generation_id IS NOT NULL THEN
        PERFORM git_catalog.require_publication_fence(repo, 'generation', work.generation_id);
    END IF;
    IF work.action = 'prepare' AND complete THEN
        IF NOT git_catalog.current_generation_approval(repo, work.generation_id) THEN
            RAISE EXCEPTION 'candidate approval changed';
        END IF;
        UPDATE git_catalog.repository_generations g SET state = 'ready'
            WHERE g.repository_id = repo AND g.generation_id = work.generation_id AND g.state = 'verified';
        IF NOT FOUND THEN RAISE EXCEPTION 'candidate is no longer verified'; END IF;
    ELSIF work.action <> 'prepare' AND NOT complete THEN
        RAISE EXCEPTION 'publication work requires completion';
    END IF;
    IF work.action = 'prepare' THEN
        INSERT INTO git_catalog.publication_packs (repository_id, filename, checksum)
            SELECT repo, component->>'name', component->>'digest'
            FROM jsonb_array_elements(work.details->'files') AS component
            ON CONFLICT (repository_id, filename) DO NOTHING;
    ELSIF work.action = 'repack' THEN
        DELETE FROM git_catalog.publication_packs WHERE repository_id = repo;
        INSERT INTO git_catalog.publication_packs (repository_id, filename, checksum)
            SELECT repo, component->>'name', component->>'digest'
            FROM jsonb_array_elements(work.details->'new') AS component;
    END IF;
    DELETE FROM git_catalog.publication_work w WHERE w.repository_id = repo AND w.operation_id = operation;
END;
$$;

CREATE FUNCTION git_catalog.confirm_repository_health(repo uuid)
RETURNS void LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
BEGIN
    PERFORM git_catalog.require_publication_fence(repo, 'coordinator', NULL);
    PERFORM git_catalog.require_publication_fence(repo, 'repository', NULL);
    IF EXISTS (SELECT 1 FROM git_catalog.publication_work WHERE repository_id = repo) THEN
        RAISE EXCEPTION 'repository still has pending publication work';
    END IF;
    DELETE FROM git_catalog.publication_blocks WHERE repository_id = repo;
END;
$$;

CREATE FUNCTION git_catalog.recover_reconcile_jobs(repo uuid)
RETURNS void LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
BEGIN
    PERFORM git_catalog.require_publication_fence(repo, 'coordinator', NULL);
    PERFORM git_catalog.require_publication_fence(repo, 'repository', NULL);
    WITH interrupted AS (
        UPDATE git_catalog.sync_jobs SET state = 'failed', outcome = 'interrupted', finished_at = clock_timestamp()
        WHERE repository_id = repo AND source_id IS NOT NULL AND state = 'running' RETURNING repository_id
    )
    INSERT INTO git_catalog.repository_incidents (repository_id, reason_code)
        SELECT repository_id, 'interrupted' FROM interrupted;
END;
$$;

CREATE FUNCTION git_catalog.promote_generation(repo uuid, generation bigint, expected bigint, rollback boolean)
RETURNS TABLE(sequence bigint, notification uuid) LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
DECLARE previous bigint;
DECLARE current_sequence bigint := 0;
BEGIN
    PERFORM git_catalog.require_publication_fence(repo, 'coordinator', NULL);
    PERFORM git_catalog.require_publication_fence(repo, 'repository', NULL);
    PERFORM git_catalog.require_publication_fence(repo, 'generation', generation);
    SELECT a.generation_id, a.publication_sequence INTO previous, current_sequence
        FROM git_catalog.active_generations a WHERE a.repository_id = repo;
    IF expected IS DISTINCT FROM coalesce(current_sequence, 0) THEN
        RAISE EXCEPTION 'publication sequence mismatch' USING ERRCODE = '40001';
    END IF;
    IF NOT git_catalog.current_generation_approval(repo, generation)
        OR EXISTS (SELECT 1 FROM git_catalog.publication_blocks WHERE repository_id = repo)
        OR EXISTS (SELECT 1 FROM git_catalog.publication_work WHERE repository_id = repo)
        OR NOT EXISTS (SELECT 1 FROM git_catalog.repository_generations g WHERE g.repository_id = repo
            AND g.generation_id = generation AND g.state = CASE WHEN rollback THEN 'retained' ELSE 'ready' END)
    THEN RAISE EXCEPTION 'publication target unavailable'; END IF;
    sequence := expected + 1;
    IF previous IS NOT NULL THEN
        PERFORM git_catalog.require_publication_fence(repo, 'generation', previous);
        UPDATE git_catalog.repository_generations SET state = 'retained', demoted_at = clock_timestamp()
            WHERE repository_id = repo AND generation_id = previous AND state = 'active'
                AND last_publication_sequence = current_sequence;
        IF NOT FOUND THEN RAISE EXCEPTION 'publication pointer inconsistent'; END IF;
    END IF;
    UPDATE git_catalog.repository_generations SET state = 'active', demoted_at = NULL,
        last_publication_sequence = sequence WHERE repository_id = repo AND generation_id = generation;
    INSERT INTO git_catalog.active_generations VALUES (repo, generation, sequence, clock_timestamp())
        ON CONFLICT (repository_id) DO UPDATE SET generation_id = excluded.generation_id,
            publication_sequence = excluded.publication_sequence, published_at = excluded.published_at;
    INSERT INTO git_catalog.notification_outbox (repository_id, generation_id, publication_sequence)
        VALUES (repo, generation, sequence) RETURNING event_id INTO notification;
    RETURN NEXT;
END;
$$;

REVOKE EXECUTE ON ALL FUNCTIONS IN SCHEMA git_catalog FROM PUBLIC;
GRANT SELECT ON git_catalog.readable_generations, git_catalog.readable_refs, git_catalog.reader_lock_keys
    TO gh_docs_serve;
GRANT SELECT ON git_catalog.source_approvals, git_catalog.active_generations, git_catalog.publication_blocks,
    git_catalog.publication_work, git_catalog.publication_packs, git_catalog.notification_outbox TO gh_docs_sync;
GRANT EXECUTE ON FUNCTION git_catalog.current_generation_approval(uuid, bigint),
    git_catalog.publication_incident(uuid, bigint, text, boolean),
    git_catalog.begin_publication_work(uuid, bigint, text, jsonb),
    git_catalog.finish_publication_work(uuid, uuid, boolean),
    git_catalog.confirm_repository_health(uuid),
    git_catalog.recover_reconcile_jobs(uuid),
    git_catalog.promote_generation(uuid, bigint, bigint, boolean) TO gh_docs_sync;
INSERT INTO git_catalog.migrations (version) VALUES (4);
