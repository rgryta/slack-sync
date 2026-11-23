-- Put the corpus where it belongs on this host: message history on spinning archive
-- storage, indexes on NVMe. The message table is append-only and rarely re-read; the
-- GIN index over `search` is what every search and history query actually touches,
-- so that is where the latency lives.
--
-- Done as a move rather than TABLESPACE clauses on CREATE, because those clauses would
-- fail outright on any database without these tablespaces — every test container, for
-- one. Here a missing tablespace simply means nothing moves.
--
-- Idempotent: `reltablespace = 0` means "inherits the database default", so an object
-- already placed is skipped and a re-run is free.
DO $$
DECLARE
  r record;
  has_archive boolean := EXISTS (SELECT 1 FROM pg_tablespace WHERE spcname = 'archive_data');
  has_nvme    boolean := EXISTS (SELECT 1 FROM pg_tablespace WHERE spcname = 'nvme_indexes');
BEGIN
  IF NOT (has_archive AND has_nvme) THEN
    RAISE NOTICE 'archive_data/nvme_indexes not present; leaving everything on the default';
    RETURN;
  END IF;

  -- Bounded, so a contended object aborts instead of queueing an ACCESS EXCLUSIVE
  -- lock in front of the ingester's writes. A skipped object is picked up next run.
  SET LOCAL lock_timeout = '5s';

  FOR r IN SELECT c.relname FROM pg_class c
            WHERE c.relnamespace = 'public'::regnamespace AND c.relkind = 'r'
              AND c.reltablespace = 0
            ORDER BY pg_relation_size(c.oid)
  LOOP
    BEGIN
      EXECUTE format('ALTER TABLE public.%I SET TABLESPACE archive_data', r.relname);
    EXCEPTION WHEN lock_not_available OR insufficient_privilege THEN
      RAISE NOTICE 'left table % where it was: %', r.relname, sqlerrm;
    END;
  END LOOP;

  FOR r IN SELECT c.relname FROM pg_class c
            WHERE c.relnamespace = 'public'::regnamespace AND c.relkind = 'i'
              AND c.reltablespace = 0
            ORDER BY pg_relation_size(c.oid)
  LOOP
    BEGIN
      EXECUTE format('ALTER INDEX public.%I SET TABLESPACE nvme_indexes', r.relname);
    EXCEPTION WHEN lock_not_available OR insufficient_privilege THEN
      RAISE NOTICE 'left index % where it was: %', r.relname, sqlerrm;
    END;
  END LOOP;
END $$;
