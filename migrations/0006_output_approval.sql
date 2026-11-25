CREATE OR REPLACE VIEW git_catalog.exploration_generations AS
    SELECT v.repository_id, v.generation_id, v.source_id, v.object_format, v.policy_version,
        v.publication_sequence, v.is_active, v.default_ref, s.canonical_identity,
        to_char(g.observed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') AS observed_at,
        (SELECT count(*) FROM git_catalog.generation_refs f
            WHERE (f.repository_id, f.generation_id) = (v.repository_id, v.generation_id)) AS ref_count,
        p.approval -> 'exploration' AS exploration
    FROM git_catalog.readable_generations v
    JOIN git_catalog.repository_generations g USING (repository_id, generation_id)
    JOIN git_catalog.repository_sources s ON (s.repository_id, s.source_id) = (v.repository_id, v.source_id)
    JOIN git_catalog.source_approvals p ON (p.source_id, p.policy_version) = (v.source_id, v.policy_version)
    WHERE p.approval -> 'exploration' -> 'version' IN ('1'::jsonb, '2'::jsonb);

GRANT SELECT ON git_catalog.exploration_generations TO gh_docs_serve;
INSERT INTO git_catalog.migrations (version) VALUES (6);
