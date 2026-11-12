-- Keep what a message used to say, and corroborate before calling one deleted.
--
-- `message.edited_at` already existed and already works: it is Slack telling us it
-- considers a message edited, on 2544 of 33676 rows. What was missing is the text it
-- replaced -- the upsert overwrote it -- and any detection of deletion at all.
--
-- `deleted_at` also already existed, with a `mark_message_deleted` writer that
-- nothing ever called: 0 rows carry it. This migration adds the column that makes
-- that writer safe to use.
--
-- No backfill. Prior texts were never stored and cannot be recovered.

CREATE TABLE message_revision (
  account_id      text        NOT NULL,
  conversation_id text        NOT NULL,
  ts              text        NOT NULL,
  seq             int         NOT NULL,
  text            text,
  -- NULL on seq 1 and only there, meaning unknown: nothing records when a message was
  -- first stored, so the earliest text held has no honest start time. Using the
  -- message's own `ts` would look tidier and claim something else entirely.
  first_seen_at   timestamptz,
  superseded_at   timestamptz,
  PRIMARY KEY (account_id, conversation_id, ts, seq),
  FOREIGN KEY (account_id, conversation_id, ts)
    REFERENCES message (account_id, conversation_id, ts) ON DELETE CASCADE
);

CREATE INDEX message_revision_seq_idx
  ON message_revision (account_id, conversation_id, ts, seq);

-- At most one current revision per message. Enforced here rather than trusted to the
-- writer: a revision log that can disagree with the message it describes leaves no
-- way to tell which of the two is lying.
CREATE UNIQUE INDEX message_revision_one_current_idx
  ON message_revision (account_id, conversation_id, ts)
  WHERE superseded_at IS NULL;

-- One clean refresh window that fails to return a message we hold stamps this; the
-- next one promotes it to deleted_at. An absence on its own is also what a short
-- page and a dropped connection look like, and deleted_at hides a message from every
-- default view, so nothing is claimed until a second pass agrees.
ALTER TABLE message ADD COLUMN missing_since timestamptz;

DO $$
DECLARE
  has_archive boolean := EXISTS (SELECT 1 FROM pg_tablespace WHERE spcname = 'archive_data');
  has_nvme    boolean := EXISTS (SELECT 1 FROM pg_tablespace WHERE spcname = 'nvme_indexes');
BEGIN
  IF NOT (has_archive AND has_nvme) THEN
    RAISE NOTICE 'archive_data/nvme_indexes absent; leaving the new objects on the default tablespace';
    RETURN;
  END IF;
  SET LOCAL lock_timeout = '5s';
  EXECUTE 'ALTER TABLE message_revision SET TABLESPACE archive_data';
  EXECUTE 'ALTER INDEX message_revision_seq_idx SET TABLESPACE nvme_indexes';
  EXECUTE 'ALTER INDEX message_revision_one_current_idx SET TABLESPACE nvme_indexes';
END $$;
