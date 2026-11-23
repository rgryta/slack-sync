-- Provision the `slack` database and its two roles.
--
-- Run as the postgres superuser:
--   psql -h <your-postgres-host> -U postgres \
--     -v ingest_pw="<ingest password>" -v mcp_pw="<mcp password>" \
--     -f deploy/provision-slack-db.sql
--
-- NOTE: pass RAW passwords. :'ingest_pw' quotes the value once; pre-quoting stores
-- the literal quotes in the role password and authentication then fails with a
-- message that says nothing about quoting.

CREATE ROLE slack_ingest LOGIN PASSWORD :'ingest_pw';
CREATE ROLE slack_mcp    LOGIN PASSWORD :'mcp_pw';

CREATE DATABASE slack OWNER slack_ingest;

\connect slack

GRANT ALL ON SCHEMA public TO slack_ingest;
GRANT USAGE ON SCHEMA public TO slack_mcp;

-- The ingester needs CREATE on the tablespaces or 0003 cannot place anything there;
-- it fails soft, so without this the corpus silently stays on the default volume.
-- Guarded because a host without these tablespaces is a valid host.
DO $$ BEGIN
  EXECUTE 'GRANT CREATE ON TABLESPACE archive_data, nvme_indexes TO slack_ingest';
EXCEPTION WHEN undefined_object THEN
  RAISE NOTICE 'archive_data/nvme_indexes absent; the corpus will live on the default tablespace';
END $$;

-- The MCP role reads everything and writes nothing. Its only write authority is the
-- two alias tables, granted by 0001_init.sql at the ingester's first boot, because
-- the tables do not exist yet at this point.
ALTER DEFAULT PRIVILEGES FOR ROLE slack_ingest IN SCHEMA public
  GRANT SELECT ON TABLES TO slack_mcp;
