-- `raw` was written double-encoded: db.py installs a jsonb codec that json.dumps the
-- value, and upsert_messages passed an already-dumped string, so every row landed as
-- a JSON *string* rather than an object. The Python round-trip was symmetric, so it
-- looked fine from the application and only broke when SQL touched the column.
UPDATE message
   SET raw = (raw #>> '{}')::jsonb
 WHERE jsonb_typeof(raw) = 'string';

-- Finding thread parents that still owe replies is a per-pass query, so it gets an
-- index. Partial: only parents claiming replies are ever candidates.
CREATE INDEX IF NOT EXISTS message_thread_parent_idx
    ON message (account_id, conversation_id, ts)
 WHERE (raw->>'reply_count') IS NOT NULL;
