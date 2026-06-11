-- Fake helper: returns hardcoded rows (stand-in for a C function reading shared memory)
CREATE OR REPLACE FUNCTION __API_SCHEMA_INTERNAL__.documentdb_stat_get_io()
RETURNS TABLE (
    database       text,
    read_count     bigint,
    write_count    bigint,
    read_bytes     bigint,
    write_bytes    bigint,
    stats_reset    timestamptz
)
LANGUAGE plpgsql
AS $$
BEGIN
    RETURN QUERY SELECT
        current_database()::text,
        42::bigint,
        17::bigint,
        1048576::bigint,
        524288::bigint,
        now()::timestamptz;
END;
$$;

-- Helper is NOT granted to PUBLIC.

-- View: the public API surface
CREATE OR REPLACE VIEW __API_CATALOG_SCHEMA__.documentdb_stat_io AS
SELECT database,
       read_count,
       write_count,
       read_bytes,
       write_bytes,
       stats_reset
FROM   __API_SCHEMA_INTERNAL__.documentdb_stat_get_io()
WHERE  current_setting(__SINGLE_QUOTED_STRING__(__API_GUC_PREFIX__) || '.track_io')::bool;

GRANT SELECT ON __API_CATALOG_SCHEMA__.documentdb_stat_io TO PUBLIC;

-- Reset function (fake: just returns void)
CREATE OR REPLACE FUNCTION __API_CATALOG_SCHEMA__.documentdb_stat_reset_io()
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    -- In a real implementation this would zero shared-memory counters.
    NULL;
END;
$$;

REVOKE EXECUTE ON FUNCTION
    __API_CATALOG_SCHEMA__.documentdb_stat_reset_io() FROM PUBLIC;
GRANT  EXECUTE ON FUNCTION
    __API_CATALOG_SCHEMA__.documentdb_stat_reset_io() TO __API_ADMIN_ROLE__;
