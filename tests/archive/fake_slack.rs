//! A Slack stand-in with the client's method surface.
//!
//! The loops are tested against this rather than against HTTP: what they must get right
//! is cursors, ordering and commit points, none of which the transport layer decides.
//! `tests/fakes.py` is the same fake in the other language, and the two agree on the
//! paging arithmetic on purpose.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{Value, json};
use slack_client::{HistoryQuery, SlackError, parse};
use slack_core::models::{Conversation, Emoji, Message, Workspace, WorkspaceUser};
use slack_ingest::Slack;

/// One recorded request. The fields the loop tests assert on, and no others.
#[derive(Debug, Clone, Default)]
pub struct Call {
    pub method: String,
    pub channel: Option<String>,
    pub oldest: Option<String>,
    pub latest: Option<String>,
    pub reserve: f64,
}

struct State {
    conversations: Vec<Value>,
    /// Held newest-first per channel when answered, exactly as Slack returns it.
    history: HashMap<String, Vec<Value>>,
    /// Replies Slack keeps out of `conversations.history`, as it does every reply not
    /// broadcast to the channel. Only `conversations.replies` returns these.
    replies: HashMap<String, Vec<Value>>,
    users: Vec<Value>,
    emoji: Value,
    page_size: usize,
    calls: Vec<Call>,
    /// `method` or `method:channel` -> the Slack error code to answer with.
    raise_on: HashMap<String, String>,
}

pub struct FakeSlack {
    pub team_id: String,
    pub my_user_id: String,
    state: Mutex<State>,
}

/// The three conversations the loop tests use: a named channel, an unnamed one, and a DM.
pub fn channels() -> Vec<Value> {
    vec![
        json!({ "id": "C0GENERAL", "name": "general", "is_channel": true, "is_member": true }),
        json!({ "id": "C0RANDOM", "name": "random", "is_channel": true, "is_member": true }),
        json!({ "id": "D0TED", "is_im": true, "user": "U0TED" }),
    ]
}

/// One raw message payload, as `tests/fakes.py`'s `message`.
pub struct Raw<'a> {
    pub ts: &'a str,
    pub user: &'a str,
    pub text: &'a str,
    pub thread_ts: Option<&'a str>,
    pub subtype: Option<&'a str>,
    pub reactions: Option<Value>,
}

impl Default for Raw<'_> {
    fn default() -> Self {
        Self {
            ts: "100.000000",
            user: "U0TED",
            text: "hello",
            thread_ts: None,
            subtype: None,
            reactions: None,
        }
    }
}

pub fn raw(message: Raw<'_>) -> Value {
    let mut payload = json!({
        "type": "message",
        "ts": message.ts,
        "user": message.user,
        "text": message.text,
    });
    let object = payload.as_object_mut().expect("an object");
    if let Some(thread_ts) = message.thread_ts {
        object.insert("thread_ts".into(), json!(thread_ts));
    }
    if let Some(subtype) = message.subtype {
        object.insert("subtype".into(), json!(subtype));
    }
    if let Some(reactions) = message.reactions {
        object.insert("reactions".into(), reactions);
    }
    payload
}

impl FakeSlack {
    pub fn new() -> Self {
        Self {
            team_id: "T0001".into(),
            my_user_id: "U0ME".into(),
            state: Mutex::new(State {
                conversations: channels(),
                history: HashMap::new(),
                replies: HashMap::new(),
                users: Vec::new(),
                emoji: json!({}),
                page_size: 100,
                calls: Vec::new(),
                raise_on: HashMap::new(),
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("the fake's state")
    }

    // ------------------------------------------------------------------- the script

    pub fn with_history(self, channel: &str, rows: Vec<Value>) -> Self {
        self.state().history.insert(channel.to_owned(), rows);
        self
    }

    pub fn with_page_size(self, size: usize) -> Self {
        self.state().page_size = size;
        self
    }

    pub fn with_users(self, users: Vec<Value>) -> Self {
        self.state().users = users;
        self
    }

    pub fn with_emoji(self, emoji: Value) -> Self {
        self.state().emoji = emoji;
        self
    }

    pub fn with_conversations(self, rows: Vec<Value>) -> Self {
        self.state().conversations = rows;
        self
    }

    pub fn set_conversations(&self, rows: Vec<Value>) {
        self.state().conversations = rows;
    }

    pub fn push_history(&self, channel: &str, row: Value) {
        self.state()
            .history
            .entry(channel.to_owned())
            .or_default()
            .push(row);
    }

    pub fn set_replies(&self, channel: &str, rows: Vec<Value>) {
        self.state().replies.insert(channel.to_owned(), rows);
    }

    /// Change a message in place — an edit, a removed reaction, a gained reply count.
    pub fn edit_message(&self, channel: &str, ts: &str, edit: impl FnOnce(&mut Value)) {
        let mut state = self.state();
        let rows = state.history.get_mut(channel).expect("a known channel");
        let row = rows
            .iter_mut()
            .find(|row| row["ts"] == ts)
            .expect("a known message");
        edit(row);
    }

    /// Answer `method` (or `method:channel`) with this Slack error code from now on.
    pub fn fail(&self, key: &str, code: &str) {
        self.state()
            .raise_on
            .insert(key.to_owned(), code.to_owned());
    }

    // --------------------------------------------------------------- what was asked

    pub fn clear_calls(&self) {
        self.state().calls.clear();
    }

    pub fn calls_to(&self, method: &str) -> Vec<Call> {
        self.state()
            .calls
            .iter()
            .filter(|call| call.method == method)
            .cloned()
            .collect()
    }

    pub fn history_calls(&self, channel: &str) -> Vec<Call> {
        self.calls_to("conversations.history")
            .into_iter()
            .filter(|call| call.channel.as_deref() == Some(channel))
            .collect()
    }

    fn record(&self, call: Call) -> Result<(), SlackError> {
        let mut state = self.state();
        let keyed = call
            .channel
            .as_ref()
            .map(|channel| format!("{}:{channel}", call.method));
        let code = state
            .raise_on
            .get(&call.method)
            .or_else(|| keyed.as_ref().and_then(|key| state.raise_on.get(key)))
            .cloned();
        state.calls.push(call.clone());
        match code {
            Some(code) => Err(SlackError::api(&call.method, &code)),
            None => Ok(()),
        }
    }
}

fn call(method: &str) -> Call {
    Call {
        method: method.to_owned(),
        ..Call::default()
    }
}

impl Slack for FakeSlack {
    async fn auth_test(&self) -> Result<(String, String), SlackError> {
        self.record(call("auth.test"))?;
        Ok((self.team_id.clone(), self.my_user_id.clone()))
    }

    async fn team_info(&self, team: Option<&str>) -> Result<Workspace, SlackError> {
        self.record(call("team.info"))?;
        Ok(Workspace {
            team_id: team.unwrap_or(&self.team_id).to_owned(),
            name: Some("Acme".into()),
            domain: Some("acme".into()),
            is_external: team.is_some(),
        })
    }

    async fn users_conversations(
        &self,
        account_id: &str,
        default_team_id: Option<&str>,
    ) -> Result<Vec<Conversation>, SlackError> {
        self.record(call("users.conversations"))?;
        let rows = self.state().conversations.clone();
        rows.iter()
            .map(|row| parse::parse_conversation(row, account_id, default_team_id))
            .collect()
    }

    async fn conversations_info(
        &self,
        channel: &str,
        account_id: &str,
        default_team_id: Option<&str>,
    ) -> Result<Conversation, SlackError> {
        self.record(Call {
            channel: Some(channel.to_owned()),
            ..call("conversations.info")
        })?;
        let rows = self.state().conversations.clone();
        let row = rows
            .iter()
            .find(|row| row["id"] == channel)
            .expect("a known conversation");
        parse::parse_conversation(row, account_id, default_team_id)
    }

    async fn conversations_history(
        &self,
        channel: &str,
        account_id: &str,
        query: HistoryQuery<'_>,
    ) -> Result<(Vec<Message>, Option<String>), SlackError> {
        self.record(Call {
            channel: Some(channel.to_owned()),
            oldest: query.oldest.map(str::to_owned),
            latest: query.latest.map(str::to_owned),
            reserve: query.reserve,
            ..call("conversations.history")
        })?;

        let (mut rows, page_size) = {
            let state = self.state();
            (
                state.history.get(channel).cloned().unwrap_or_default(),
                state.page_size,
            )
        };
        // Newest first, exactly as Slack returns it. String comparison throughout: a
        // Slack ts is fixed-width, so lexical and chronological order agree.
        rows.sort_by_key(|row| std::cmp::Reverse(text(row, "ts")));
        if let Some(oldest) = query.oldest {
            rows.retain(|row| text(row, "ts").as_str() > oldest);
        }
        if let Some(latest) = query.latest {
            rows.retain(|row| text(row, "ts").as_str() < latest);
        }

        let start: usize = match query.cursor {
            Some(cursor) => cursor.parse().expect("the fake's own cursor"),
            None => 0,
        };
        let size = (query.limit as usize).min(page_size);
        let end = (start + size).min(rows.len());
        let page = &rows[start.min(rows.len())..end];
        let next = (end < rows.len()).then(|| end.to_string());
        let messages = page
            .iter()
            .map(|row| parse::parse_message(row, account_id, channel))
            .collect::<Result<Vec<_>, _>>()?;
        Ok((messages, next))
    }

    async fn conversations_replies(
        &self,
        channel: &str,
        thread_ts: &str,
        account_id: &str,
        reserve: f64,
    ) -> Result<Vec<Message>, SlackError> {
        self.record(Call {
            channel: Some(channel.to_owned()),
            reserve,
            ..call("conversations.replies")
        })?;
        let mut rows: Vec<Value> = {
            let state = self.state();
            state
                .history
                .get(channel)
                .into_iter()
                .chain(state.replies.get(channel))
                .flatten()
                .filter(|row| {
                    row["ts"] == thread_ts || row.get("thread_ts").is_some_and(|t| t == thread_ts)
                })
                .cloned()
                .collect()
        };
        rows.sort_by_key(|row| text(row, "ts"));
        rows.iter()
            .map(|row| parse::parse_message(row, account_id, channel))
            .collect()
    }

    async fn users_list(&self, account_id: &str) -> Result<Vec<WorkspaceUser>, SlackError> {
        self.record(call("users.list"))?;
        let rows = self.state().users.clone();
        rows.iter()
            .map(|row| parse::parse_user(row, account_id))
            .collect()
    }

    async fn emoji_list(&self, team_id: &str) -> Result<Vec<Emoji>, SlackError> {
        self.record(call("emoji.list"))?;
        let emoji = self.state().emoji.clone();
        Ok(parse::parse_emoji(&emoji, team_id))
    }
}

fn text(row: &Value, key: &str) -> String {
    row[key].as_str().unwrap_or_default().to_owned()
}
