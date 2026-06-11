-- Test: documentdb_stat_io conventions verification
-- This test validates that the stat_io example follows RFC-0007 conventions.

-- 1. Column shape: verify view columns and types
SELECT column_name, data_type
FROM   information_schema.columns
WHERE  table_schema = 'documentdb_api_catalog'
  AND  table_name   = 'documentdb_stat_io'
ORDER  BY ordinal_position;

-- 2. GUC on (default) → view returns data
SET documentdb.track_io = true;
SELECT count(*) > 0 AS has_rows FROM documentdb_api_catalog.documentdb_stat_io;

-- 3. GUC off → view returns empty result set
SET documentdb.track_io = false;
SELECT count(*) AS row_count FROM documentdb_api_catalog.documentdb_stat_io;

-- Reset GUC
SET documentdb.track_io = true;

-- 4. Permission: PUBLIC can SELECT the view
SET ROLE documentdb_readonly_role;
SELECT count(*) >= 0 AS can_select FROM documentdb_api_catalog.documentdb_stat_io;
RESET ROLE;

-- 5. Permission: non-admin cannot call reset
SET ROLE documentdb_readonly_role;
SELECT documentdb_api_catalog.documentdb_stat_reset_io();
RESET ROLE;

-- 6. Permission: admin CAN call reset
SET ROLE documentdb_admin_role;
SELECT documentdb_api_catalog.documentdb_stat_reset_io();
RESET ROLE;

-- 7. Helper is NOT callable by PUBLIC
SET ROLE documentdb_readonly_role;
SELECT * FROM documentdb_api_internal.documentdb_stat_get_io();
RESET ROLE;
