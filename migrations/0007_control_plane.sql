ALTER TABLE git_catalog.sync_jobs ADD COLUMN scheduler_retry_after bigint CHECK (scheduler_retry_after >= 0);

CREATE TABLE git_catalog.sync_schedule (
    repository_id uuid PRIMARY KEY REFERENCES git_catalog.repositories,
    enrolled boolean NOT NULL DEFAULT true,
    paused boolean NOT NULL DEFAULT false,
    next_run_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    last_hint_digest text,
    rollback_digest bytea CHECK (octet_length(rollback_digest) = 32),
    last_hint_at bigint
);

CREATE TABLE git_catalog.scheduled_jobs (
    job_id uuid PRIMARY KEY DEFAULT uuidv7(),
    repository_id uuid NOT NULL REFERENCES git_catalog.sync_schedule,
    state text NOT NULL DEFAULT 'queued' CHECK (state IN ('queued','active','finished')),
    reason text NOT NULL CHECK (reason IN ('periodic','feed','resume')),
    next_attempt_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    expected_sequence bigint CHECK (expected_sequence >= 0),
    owner_pid integer,
    attempt_id uuid REFERENCES git_catalog.sync_jobs,
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts BETWEEN 0 AND 3),
    outcome text,
    finished_at timestamptz,
    CHECK ((state = 'finished') = (finished_at IS NOT NULL))
);
CREATE UNIQUE INDEX scheduled_one_queued ON git_catalog.scheduled_jobs(repository_id) WHERE state = 'queued';
CREATE UNIQUE INDEX scheduled_one_active ON git_catalog.scheduled_jobs(repository_id) WHERE state = 'active';
CREATE INDEX scheduled_finished ON git_catalog.scheduled_jobs(finished_at) WHERE state = 'finished';
CREATE INDEX scheduled_due ON git_catalog.scheduled_jobs(next_attempt_at, job_id) WHERE state <> 'finished';

CREATE TABLE git_catalog.hint_receipts (
    event_id uuid PRIMARY KEY CHECK (uuid_extract_version(event_id) IS NOT DISTINCT FROM 7),
    repository_id uuid NOT NULL REFERENCES git_catalog.sync_schedule,
    digest text NOT NULL CHECK (digest ~ '^[0-9a-f]{64}$'),
    observed_at bigint NOT NULL,
    expires_at timestamptz NOT NULL
);
CREATE INDEX hint_repo_expiry ON git_catalog.hint_receipts(repository_id, expires_at);
CREATE INDEX hint_expiry ON git_catalog.hint_receipts(expires_at);

ALTER TABLE git_catalog.notification_outbox ADD COLUMN delivery_attempts integer NOT NULL DEFAULT 0
    CHECK (delivery_attempts >= 0);
ALTER TABLE git_catalog.notification_outbox ADD COLUMN next_attempt_at timestamptz NOT NULL DEFAULT clock_timestamp();
CREATE INDEX notification_due ON git_catalog.notification_outbox(next_attempt_at, event_id)
    WHERE delivered_at IS NULL;

-- The negative bigint key is reserved for the single deployment scheduler.
CREATE FUNCTION git_catalog.scheduler_owns(backend integer) RETURNS boolean
LANGUAGE sql VOLATILE SET search_path = pg_catalog AS $$
    SELECT EXISTS (SELECT 1 FROM pg_locks WHERE pid = backend AND locktype = 'advisory'
        AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
        AND classid = '4294967295'::oid AND objid = '4294967293'::oid
        AND objsubid = 1 AND mode = 'ExclusiveLock' AND granted)
$$;

CREATE FUNCTION git_catalog.require_scheduler() RETURNS void
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
BEGIN
    IF NOT git_catalog.scheduler_owns(pg_backend_pid()) THEN
        RAISE EXCEPTION 'scheduler ownership required';
    END IF;
END;
$$;

CREATE FUNCTION git_catalog.configure_schedule(roster uuid[]) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
BEGIN
    PERFORM git_catalog.require_scheduler();
    IF cardinality(roster) > 128 OR array_position(roster, NULL) IS NOT NULL THEN
        RAISE EXCEPTION 'invalid scheduler roster';
    END IF;
    UPDATE git_catalog.sync_schedule SET enrolled = false
        WHERE enrolled AND NOT repository_id = ANY(roster);
    INSERT INTO git_catalog.sync_schedule(repository_id)
        SELECT r.repository_id FROM git_catalog.repositories r WHERE r.repository_id = ANY(roster)
        ON CONFLICT (repository_id) DO UPDATE SET enrolled = true,
            next_run_at = clock_timestamp() WHERE NOT sync_schedule.enrolled;
    UPDATE git_catalog.scheduled_jobs j
        SET state = 'finished', outcome = 'removed', finished_at = clock_timestamp()
        FROM git_catalog.sync_schedule s WHERE s.repository_id = j.repository_id
        AND NOT s.enrolled AND j.state <> 'finished';
END;
$$;

CREATE FUNCTION git_catalog.enqueue_hint(event uuid, repo uuid, token text, observation bigint)
RETURNS boolean LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
DECLARE schedule git_catalog.sync_schedule%ROWTYPE;
DECLARE receipt git_catalog.hint_receipts%ROWTYPE;
DECLARE now_seconds bigint := floor(extract(epoch FROM clock_timestamp()));
BEGIN
    PERFORM git_catalog.require_scheduler();
    IF observation <= now_seconds - 86400 OR observation > now_seconds + 60 THEN
        RETURN false;
    END IF;
    SELECT * INTO schedule FROM git_catalog.sync_schedule s WHERE s.repository_id = repo FOR UPDATE;
    IF NOT FOUND OR NOT schedule.enrolled OR NOT EXISTS (
        SELECT 1 FROM git_catalog.repositories r JOIN git_catalog.repository_sources src USING (repository_id)
        WHERE r.repository_id = repo AND r.approval_status = 'approved' AND src.acquisition_method = 'https'
    ) THEN RETURN false; END IF;
    SELECT * INTO receipt FROM git_catalog.hint_receipts WHERE event_id = event;
    IF FOUND THEN
        RETURN (receipt.repository_id, receipt.digest, receipt.observed_at) = (repo, token, observation);
    END IF;
    IF schedule.paused THEN RETURN false; END IF;
    DELETE FROM git_catalog.hint_receipts WHERE repository_id = repo AND expires_at <= clock_timestamp();
    IF (SELECT count(*) FROM git_catalog.hint_receipts WHERE repository_id = repo) >= 1024 THEN
        RETURN false;
    END IF;
    INSERT INTO git_catalog.hint_receipts VALUES (event, repo, token, observation,
        to_timestamp(observation + 86400));
    IF (schedule.last_hint_at IS NULL OR observation >= schedule.last_hint_at)
        AND schedule.last_hint_digest IS DISTINCT FROM token THEN
        INSERT INTO git_catalog.scheduled_jobs(repository_id, reason) VALUES (repo, 'feed')
            ON CONFLICT (repository_id) WHERE state = 'queued' DO NOTHING;
    END IF;
    IF schedule.last_hint_at IS NULL OR observation >= schedule.last_hint_at THEN
        UPDATE git_catalog.sync_schedule SET last_hint_digest = token, last_hint_at = observation
            WHERE repository_id = repo;
    END IF;
    RETURN true;
END;
$$;

CREATE FUNCTION git_catalog.schedule_periodic(seconds integer) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
BEGIN
    PERFORM git_catalog.require_scheduler();
    IF seconds NOT BETWEEN 1 AND 95040 THEN RAISE EXCEPTION 'invalid schedule interval'; END IF;
    DELETE FROM git_catalog.hint_receipts WHERE expires_at <= clock_timestamp();
    DELETE FROM git_catalog.scheduled_jobs WHERE state = 'finished' AND finished_at < clock_timestamp() - interval '30 days';
    UPDATE git_catalog.scheduled_jobs j SET state = 'finished', outcome = 'approval_unavailable',
        finished_at = clock_timestamp()
        FROM git_catalog.repositories r WHERE r.repository_id = j.repository_id
        AND r.approval_status <> 'approved' AND j.state <> 'finished';
    WITH due AS (
        UPDATE git_catalog.sync_schedule s SET next_run_at = clock_timestamp() +
            make_interval(secs => seconds + floor(random() * seconds / 10))
        FROM git_catalog.repositories r WHERE r.repository_id = s.repository_id
            AND r.approval_status = 'approved' AND s.enrolled AND NOT s.paused
            AND s.next_run_at <= clock_timestamp()
            AND EXISTS (SELECT 1 FROM git_catalog.repository_sources src WHERE src.repository_id = r.repository_id
                AND src.acquisition_method = 'https')
        RETURNING s.repository_id
    )
    INSERT INTO git_catalog.scheduled_jobs(repository_id, reason)
        SELECT repository_id, 'periodic' FROM due
        ON CONFLICT (repository_id) WHERE state = 'queued' DO NOTHING;
END;
$$;

CREATE FUNCTION git_catalog.claim_scheduled(excluded uuid[]) RETURNS SETOF git_catalog.scheduled_jobs
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
DECLARE chosen git_catalog.scheduled_jobs%ROWTYPE;
BEGIN
    PERFORM git_catalog.require_scheduler();
    SELECT j.* INTO chosen
        FROM git_catalog.scheduled_jobs j JOIN git_catalog.sync_schedule s USING (repository_id)
        JOIN git_catalog.repositories r USING (repository_id)
        WHERE s.enrolled AND NOT s.paused AND r.approval_status = 'approved'
            AND j.state <> 'finished' AND j.next_attempt_at <= clock_timestamp()
            AND NOT j.job_id = ANY(excluded)
            AND (j.state = 'active' OR NOT EXISTS (SELECT 1 FROM git_catalog.scheduled_jobs a
                WHERE a.repository_id = j.repository_id AND a.state = 'active'))
        ORDER BY j.next_attempt_at, j.job_id LIMIT 1 FOR UPDATE OF s, j;
    IF FOUND THEN
        RETURN QUERY UPDATE git_catalog.scheduled_jobs j
            SET state = 'active', owner_pid = pg_backend_pid(),
                expected_sequence = coalesce(j.expected_sequence, (SELECT a.publication_sequence
                    FROM git_catalog.active_generations a WHERE a.repository_id = j.repository_id), 0),
                attempts = greatest(j.attempts, 1)
            WHERE j.job_id = chosen.job_id RETURNING j.*;
    END IF;
END;
$$;

CREATE FUNCTION git_catalog.start_scheduled_reconcile(scheduled uuid, repo uuid, source uuid, version bigint)
RETURNS uuid LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
DECLARE job uuid;
DECLARE owner integer;
BEGIN
    SELECT j.owner_pid INTO owner FROM git_catalog.scheduled_jobs j JOIN git_catalog.sync_schedule s USING (repository_id)
        WHERE j.job_id = scheduled AND j.repository_id = repo AND j.state = 'active'
            AND s.enrolled AND NOT s.paused FOR UPDATE OF s, j;
    IF NOT FOUND OR NOT git_catalog.scheduler_owns(owner) THEN
        RAISE EXCEPTION 'scheduled reconciliation is no longer authorized';
    END IF;
    PERFORM git_catalog.require_publication_fence(repo, 'coordinator', NULL);
    job := git_catalog.start_reconcile(repo, source, version);
    UPDATE git_catalog.scheduled_jobs SET attempt_id = job WHERE job_id = scheduled;
    RETURN job;
END;
$$;

CREATE FUNCTION git_catalog.fail_scheduled_reconcile(job uuid, reason text, attempts integer, retry_after bigint)
RETURNS boolean LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
BEGIN
    IF NOT git_catalog.fail_reconcile(job, reason, attempts) THEN RETURN false; END IF;
    UPDATE git_catalog.sync_jobs SET scheduler_retry_after = retry_after WHERE job_id = job;
    RETURN true;
END;
$$;

CREATE FUNCTION git_catalog.seal_scheduled_candidate(scheduled uuid, job uuid, logical bytea,
    observed bytea, pack bytea, default_name bytea, snapshot jsonb, proof jsonb, observed_time text, attempt_count integer)
RETURNS bigint LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
DECLARE owner integer;
BEGIN
    SELECT j.owner_pid INTO owner FROM git_catalog.scheduled_jobs j JOIN git_catalog.sync_schedule s USING (repository_id)
        WHERE j.job_id = scheduled AND j.attempt_id = job AND j.state = 'active' AND s.enrolled AND NOT s.paused
        FOR UPDATE OF s, j;
    IF NOT FOUND OR NOT git_catalog.scheduler_owns(owner) THEN
        RAISE EXCEPTION 'scheduled candidate sealing is no longer authorized';
    END IF;
    RETURN git_catalog.seal_candidate(job, logical, observed, pack, default_name, snapshot, proof, observed_time, attempt_count);
END;
$$;

CREATE FUNCTION git_catalog.finish_scheduled(scheduled uuid, result text, retry_seconds bigint)
RETURNS void LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
BEGIN
    PERFORM git_catalog.require_scheduler();
    IF result !~ '^[a-z][a-z0-9_]{0,63}$' OR retry_seconds NOT BETWEEN 0 AND 253402300799 THEN
        RAISE EXCEPTION 'invalid scheduler outcome or delay';
    END IF;
    UPDATE git_catalog.scheduled_jobs SET state = CASE WHEN retry_seconds > 0 AND attempts < 3
            THEN 'active' ELSE 'finished' END,
        outcome = result,
        finished_at = CASE WHEN retry_seconds > 0 AND attempts < 3 THEN NULL ELSE clock_timestamp() END,
        next_attempt_at = clock_timestamp() + make_interval(secs => retry_seconds::double precision),
        attempts = CASE WHEN retry_seconds > 0 AND attempts < 3 THEN attempts + 1 ELSE attempts END,
        attempt_id = CASE WHEN retry_seconds > 0 AND attempts < 3 AND result <> 'publication_retry'
            THEN NULL ELSE attempt_id END
        WHERE job_id = scheduled AND state = 'active' AND owner_pid = pg_backend_pid();
END;
$$;

CREATE FUNCTION git_catalog.change_schedule(repo uuid, pause boolean, expected bigint) RETURNS boolean
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
BEGIN
    PERFORM 1 FROM git_catalog.sync_schedule WHERE repository_id = repo AND enrolled FOR UPDATE;
    IF NOT FOUND THEN RETURN false; END IF;
    IF NOT pause AND expected IS DISTINCT FROM coalesce((SELECT publication_sequence
        FROM git_catalog.active_generations WHERE repository_id = repo), 0) THEN
        RETURN false;
    END IF;
    UPDATE git_catalog.sync_schedule SET paused = pause, next_run_at = CASE WHEN pause THEN next_run_at
        ELSE clock_timestamp() + interval '24 hours' END WHERE repository_id = repo;
    IF NOT pause THEN
        UPDATE git_catalog.scheduled_jobs SET state = 'finished', outcome = 'resumed',
            finished_at = clock_timestamp() WHERE repository_id = repo AND state = 'active';
        INSERT INTO git_catalog.scheduled_jobs(repository_id, reason) VALUES (repo, 'resume')
            ON CONFLICT (repository_id) WHERE state = 'queued' DO UPDATE
            SET reason = 'resume', next_attempt_at = clock_timestamp();
    END IF;
    RETURN true;
END;
$$;

CREATE FUNCTION git_catalog.release_scheduler_fences(repo uuid) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
DECLARE key bigint;
BEGIN
    PERFORM git_catalog.require_scheduler();
    FOR key IN SELECT k.advisory_key FROM git_catalog.lock_keys k WHERE k.repository_id = repo
        AND EXISTS (SELECT 1 FROM pg_locks l WHERE l.pid = pg_backend_pid() AND l.locktype = 'advisory'
            AND l.objsubid = 1 AND l.granted AND l.mode = 'ExclusiveLock'
            AND l.classid::bigint = (k.advisory_key >> 32) AND l.objid::bigint = (k.advisory_key & 4294967295))
    LOOP
        -- Release reentrant acquisitions as well as the initial acquisition.
        WHILE EXISTS (SELECT 1 FROM pg_locks l WHERE l.pid = pg_backend_pid() AND l.locktype = 'advisory'
            AND l.objsubid = 1 AND l.granted AND l.mode = 'ExclusiveLock'
            AND l.classid::bigint = (key >> 32) AND l.objid::bigint = (key & 4294967295))
        LOOP
            PERFORM pg_advisory_unlock(key);
        END LOOP;
    END LOOP;
END;
$$;

CREATE FUNCTION git_catalog.record_delivery(event uuid, retry_seconds integer) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
BEGIN
    PERFORM git_catalog.require_scheduler();
    IF retry_seconds NOT BETWEEN 0 AND 330 THEN RAISE EXCEPTION 'invalid notification retry delay'; END IF;
    UPDATE git_catalog.notification_outbox
        SET delivery_attempts = CASE WHEN delivery_attempts < 2147483647 THEN delivery_attempts + 1 ELSE delivery_attempts END,
            delivered_at = CASE WHEN retry_seconds = 0 THEN clock_timestamp() ELSE NULL END,
            next_attempt_at = clock_timestamp() + make_interval(secs => retry_seconds)
        WHERE event_id = event AND delivered_at IS NULL;
END;
$$;

CREATE FUNCTION git_catalog.observe_notification(event uuid, repo uuid, generation bigint, sequence bigint)
RETURNS text LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
    SELECT CASE WHEN a.publication_sequence > e.publication_sequence THEN 'superseded'
        WHEN a.publication_sequence = e.publication_sequence AND a.generation_id = e.generation_id
            THEN 'observed' END
    FROM git_catalog.notification_outbox e JOIN git_catalog.active_generations a USING (repository_id)
    WHERE e.event_id = event AND e.repository_id = repo AND e.generation_id = generation
        AND e.publication_sequence = sequence
$$;

CREATE OR REPLACE FUNCTION git_catalog.promote_generation(repo uuid, generation bigint, expected bigint, rollback boolean)
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
    -- Serialize sequence changes with pause and resume.
    PERFORM 1 FROM git_catalog.sync_schedule WHERE repository_id = repo FOR UPDATE;
    IF rollback THEN
        UPDATE git_catalog.sync_schedule SET paused = true, rollback_digest =
            (SELECT p.logical_digest FROM git_catalog.private_candidates p
                WHERE p.repository_id = repo AND p.generation_id = previous)
            WHERE repository_id = repo AND enrolled;
    ELSE
        UPDATE git_catalog.sync_schedule SET rollback_digest = NULL
            WHERE repository_id = repo AND rollback_digest IS NOT NULL;
    END IF;
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

CREATE FUNCTION git_catalog.promote_scheduled(scheduled uuid, repo uuid, generation bigint, expected bigint)
RETURNS TABLE(sequence bigint, notification uuid)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
BEGIN
    PERFORM git_catalog.require_scheduler();
    PERFORM 1 FROM git_catalog.scheduled_jobs j JOIN git_catalog.sync_schedule s USING (repository_id)
        WHERE j.job_id = scheduled AND j.repository_id = repo AND j.state = 'active'
            AND j.owner_pid = pg_backend_pid() AND j.expected_sequence = expected AND s.enrolled AND NOT s.paused
            AND EXISTS (SELECT 1 FROM git_catalog.sync_jobs a JOIN git_catalog.private_candidates p
                ON p.repository_id = a.repository_id AND p.generation_id = generation
                WHERE p.logical_digest IS DISTINCT FROM s.rollback_digest AND a.job_id = j.attempt_id AND a.state = 'succeeded' AND p.state = 'usable'
                    AND (p.job_id = a.job_id OR a.outcome = 'no_change'
                        AND a.observation->>'baseline_generation' = generation::text))
        FOR UPDATE OF s, j;
    IF NOT FOUND THEN RAISE EXCEPTION 'scheduled promotion is no longer authorized'; END IF;
    RETURN QUERY SELECT * FROM git_catalog.promote_generation(repo, generation, expected, false);
    PERFORM git_catalog.finish_scheduled(scheduled, 'published', 0);
END;
$$;

REVOKE EXECUTE ON ALL FUNCTIONS IN SCHEMA git_catalog FROM PUBLIC;
GRANT SELECT ON git_catalog.sync_schedule, git_catalog.scheduled_jobs TO gh_docs_sync;
GRANT EXECUTE ON FUNCTION git_catalog.configure_schedule(uuid[]),
    git_catalog.enqueue_hint(uuid, uuid, text, bigint), git_catalog.schedule_periodic(integer),
    git_catalog.claim_scheduled(uuid[]), git_catalog.start_scheduled_reconcile(uuid, uuid, uuid, bigint),
    git_catalog.fail_scheduled_reconcile(uuid, text, integer, bigint),
    git_catalog.seal_scheduled_candidate(uuid, uuid, bytea, bytea, bytea, bytea, jsonb, jsonb, text, integer),
    git_catalog.finish_scheduled(uuid, text, bigint), git_catalog.change_schedule(uuid, boolean, bigint),
    git_catalog.release_scheduler_fences(uuid), git_catalog.record_delivery(uuid, integer),
    git_catalog.promote_scheduled(uuid, uuid, bigint, bigint) TO gh_docs_sync;
GRANT EXECUTE ON FUNCTION git_catalog.observe_notification(uuid, uuid, bigint, bigint) TO gh_docs_serve;
INSERT INTO git_catalog.migrations (version) VALUES (7);
