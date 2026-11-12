-- Give a DM the person on the other end.
--
-- A DM has no name, so `conversation_label` could only ever render it as
-- `im:D09QVUDMLGM`. Slack's `users.conversations` carries the counterpart on every
-- `im` entry; nothing was keeping it.
ALTER TABLE conversation ADD COLUMN IF NOT EXISTS counterpart_user_id text;

-- Backfill what the corpus can already prove: the one author in a DM who is not the
-- account holder. This covers the DMs that have been spoken in; the rest fill in on
-- the next discovery pass, which is the authoritative source.
UPDATE conversation c
SET counterpart_user_id = sub.user_id
FROM (
  SELECT DISTINCT ON (m.account_id, m.conversation_id)
         m.account_id, m.conversation_id, m.user_id
    FROM message m
    JOIN conversation cc ON cc.account_id = m.account_id AND cc.id = m.conversation_id
    JOIN account a ON a.id = m.account_id
   WHERE cc.type = 'im'
     AND m.user_id IS NOT NULL
     AND m.user_id <> a.my_user_id
   ORDER BY m.account_id, m.conversation_id, m.posted_at
) AS sub
WHERE c.account_id = sub.account_id
  AND c.id = sub.conversation_id
  AND c.type = 'im'
  AND c.counterpart_user_id IS NULL;

CREATE INDEX IF NOT EXISTS conversation_counterpart_idx
  ON conversation (account_id, counterpart_user_id)
  WHERE counterpart_user_id IS NOT NULL;
