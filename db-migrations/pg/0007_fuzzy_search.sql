-- Search that forgives accents, typos and word endings.
--
-- 'simple' matched whole words exactly: "zolw" missed "żółw", "samochod" missed
-- "samochodem", a typo missed everything. Two changes, both in stock contrib:
--
--   * `simple_fold` is 'simple' behind `unaccent`, so the tsvector and the query fold
--     the same way. Exact-word search keeps its meaning, minus the accents.
--   * a trigram index over the folded text backs `%>` (pg_trgm word similarity), which
--     the query uses per word to catch a typo or a different ending of the same word.
--
-- `search_fold` exists because `unaccent(text)` is only STABLE (it looks its dictionary
-- up by name); the two-argument form with a schema-qualified dictionary may be declared
-- IMMUTABLE, which an index expression requires.
CREATE EXTENSION IF NOT EXISTS unaccent;
CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE OR REPLACE FUNCTION search_fold(text) RETURNS text
  LANGUAGE sql IMMUTABLE PARALLEL SAFE STRICT
  RETURN lower(public.unaccent('public.unaccent'::regdictionary, $1));

DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_ts_config WHERE cfgname = 'simple_fold') THEN
    CREATE TEXT SEARCH CONFIGURATION simple_fold (COPY = simple);
    ALTER TEXT SEARCH CONFIGURATION simple_fold
      ALTER MAPPING FOR word, hword, hword_part WITH public.unaccent, simple;
  END IF;
END $$;

-- One rewrite of the table; the GIN index over `search` is rebuilt where it is.
ALTER TABLE message ALTER COLUMN search
  SET EXPRESSION AS (to_tsvector('simple_fold', coalesce(text, '')));

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_tablespace WHERE spcname = 'nvme_indexes') THEN
    CREATE INDEX IF NOT EXISTS message_fold_trgm_idx ON message
      USING gin (search_fold(text) gin_trgm_ops) TABLESPACE nvme_indexes;
  ELSE
    CREATE INDEX IF NOT EXISTS message_fold_trgm_idx ON message
      USING gin (search_fold(text) gin_trgm_ops);
  END IF;
END $$;
