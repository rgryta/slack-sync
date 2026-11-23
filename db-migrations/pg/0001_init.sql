-- slack-sync initial schema.
--
-- `ts` is text, not a timestamp: Slack's `ts` is an opaque message identifier
-- ("1723640400.001200") that doubles as a sort key. Parsing it to numeric would make
-- the primary key lossy against Slack's own identity. `posted_at` carries the parsed
-- value for range queries and indexing.

CREATE TABLE workspace (
  team_id      text PRIMARY KEY,
  name         text,
  domain       text,
  is_external  boolean NOT NULL DEFAULT false,
  first_seen   timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE account (
  id                   text PRIMARY KEY,
  team_id              text NOT NULL REFERENCES workspace(team_id),
  my_user_id           text NOT NULL,
  enabled              boolean NOT NULL DEFAULT true,
  last_auth_error      text,
  last_auth_error_at   timestamptz,
  updated_at           timestamptz NOT NULL DEFAULT now()
);

-- `reactions_fresh_since`, not `..._until`: it stores now() - window, the oldest point
-- from which reaction counts are current. Named `until` it reads as the opposite and
-- every comparison against it inverts.
CREATE TABLE conversation (
  account_id            text NOT NULL REFERENCES account(id),
  id                    text NOT NULL,
  team_id               text REFERENCES workspace(team_id),
  shared_team_ids       text[] NOT NULL DEFAULT '{}',
  type                  text NOT NULL CHECK (type IN ('im','mpim','channel','group')),
  name                  text,
  is_member             boolean NOT NULL DEFAULT false,
  in_scope              boolean NOT NULL DEFAULT false,
  scope_reason          text CHECK (scope_reason IN ('dm','group_dm','named_channel','mention_thread')),
  poll_cursor           text,
  backfill_cursor       text,
  backfill_done         boolean NOT NULL DEFAULT false,
  reactions_fresh_since timestamptz,
  last_polled_at        timestamptz,
  PRIMARY KEY (account_id, id)
);

-- to_tsvector('simple', ...) — no stemming. The corpus is mixed-language; 'english'
-- would silently drop hits on non-English text.
CREATE TABLE message (
  account_id      text NOT NULL,
  conversation_id text NOT NULL,
  ts              text NOT NULL,
  thread_ts       text,
  user_id         text,
  text            text NOT NULL DEFAULT '',
  subtype         text,
  edited_at       timestamptz,
  deleted_at      timestamptz,
  raw             jsonb NOT NULL,
  posted_at       timestamptz NOT NULL,
  search          tsvector GENERATED ALWAYS AS (to_tsvector('simple', coalesce(text,''))) STORED,
  PRIMARY KEY (account_id, conversation_id, ts),
  FOREIGN KEY (account_id, conversation_id) REFERENCES conversation(account_id, id)
);
CREATE INDEX message_search_idx   ON message USING gin (search);
CREATE INDEX message_thread_idx   ON message (account_id, conversation_id, thread_ts);
CREATE INDEX message_posted_idx   ON message (posted_at DESC);
CREATE INDEX message_user_idx     ON message (account_id, user_id);

CREATE TABLE reaction (
  account_id      text NOT NULL,
  conversation_id text NOT NULL,
  message_ts      text NOT NULL,
  name            text NOT NULL,
  user_ids        text[] NOT NULL DEFAULT '{}',
  count           integer NOT NULL DEFAULT 0,
  observed_at     timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (account_id, conversation_id, message_ts, name),
  FOREIGN KEY (account_id, conversation_id, message_ts)
    REFERENCES message(account_id, conversation_id, ts) ON DELETE CASCADE
);

CREATE TABLE emoji (
  team_id    text NOT NULL REFERENCES workspace(team_id),
  name       text NOT NULL,
  url        text,
  alias_for  text,
  updated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (team_id, name)
);

CREATE TABLE mention_thread (
  account_id      text NOT NULL,
  conversation_id text NOT NULL,
  thread_ts       text NOT NULL,
  mention_ts      text NOT NULL,
  first_seen      timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (account_id, conversation_id, thread_ts),
  FOREIGN KEY (account_id, conversation_id) REFERENCES conversation(account_id, id)
);

CREATE TABLE workspace_user (
  account_id text NOT NULL REFERENCES account(id),
  user_id    text NOT NULL,
  team_id    text,
  name       text,
  real_name  text,
  is_bot     boolean NOT NULL DEFAULT false,
  updated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (account_id, user_id)
);

CREATE TABLE user_alias (
  alias      text PRIMARY KEY,
  note       text,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now()
);

-- No account FK: an alias may name a user in an account before that account's user
-- list has been synced, and must survive a user disappearing from workspace_user.
CREATE TABLE user_alias_member (
  account_id text NOT NULL,
  user_id    text NOT NULL,
  alias      text NOT NULL REFERENCES user_alias(alias) ON DELETE CASCADE,
  PRIMARY KEY (account_id, user_id)
);
CREATE INDEX user_alias_member_alias_idx ON user_alias_member (alias);

CREATE TABLE sync_state (
  account_id              text NOT NULL REFERENCES account(id),
  loop                    text NOT NULL CHECK (loop IN ('poll','backfill')),
  current_conversation_id text,
  last_run_at             timestamptz,
  last_error              text,
  last_error_at           timestamptz,
  PRIMARY KEY (account_id, loop)
);

-- The MCP role's only write authority. GRANT is not directly executable inside a DO
-- block (it must go through EXECUTE), and the role does not exist on a throwaway test
-- container, so the missing-role case is swallowed.
DO $$ BEGIN
  EXECUTE 'GRANT INSERT, UPDATE, DELETE ON user_alias, user_alias_member TO slack_mcp';
EXCEPTION WHEN undefined_object THEN NULL;
END $$;
