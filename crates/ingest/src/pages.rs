//! Writing one page of history, shared by the poll and backfill loops.
//!
//! Both loops classify and store identically and differ only in which direction they
//! page and which cursor they advance, so the page write lives here rather than in one
//! loop with the other importing it sideways.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use slack_core::models::{Conversation, Message, ScopeReason};
use slack_core::store;
use sqlx::postgres::PgPool;

use crate::api::Slack;
use crate::error::IngestError;
use crate::filter::{Action, classify};

/// What the filter needs, carried across a pass.
///
/// `known_mention_threads` is held in memory for the whole pass: it is the fourth scope
/// rule and must not cost a query per message. It is shared between the two loops behind
/// a mutex, as the one Python object was shared between two tasks — the poll pass
/// replaces it from the database and both loops add to it, and a thread the backfill
/// discovers has to be visible to the poll before its own next pass.
#[derive(Clone, Debug)]
pub struct IngestContext {
    pub account_id: String,
    pub my_user_id: String,
    pub scope_channels: HashSet<String>,
    known_mention_threads: Arc<Mutex<HashSet<(String, String)>>>,
}

impl IngestContext {
    pub fn new(account_id: String, my_user_id: String, scope_channels: HashSet<String>) -> Self {
        Self {
            account_id,
            my_user_id,
            scope_channels,
            known_mention_threads: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// The known set, locked. Held only across the classify loop, which does no I/O.
    pub fn known_threads(&self) -> MutexGuard<'_, HashSet<(String, String)>> {
        self.known_mention_threads
            .lock()
            .expect("the mention-thread set")
    }

    pub fn replace_known_threads(&self, keys: HashSet<(String, String)>) {
        *self.known_threads() = keys;
    }

    fn remember_threads(&self, conversation_id: &str, keys: &HashSet<String>) {
        let mut known = self.known_threads();
        for key in keys {
            known.insert((conversation_id.to_owned(), key.clone()));
        }
    }
}

#[derive(Debug, Default)]
pub struct PageResult {
    pub stored: Vec<Message>,
    pub new_threads: HashSet<String>,
    pub touched_threads: HashSet<String>,
    /// thread key -> the ts of the message that actually carried the mention. Derived
    /// while classifying rather than searched for afterwards: a page also contains the
    /// thread parent and other replies, and any of them would match the key.
    pub mention_of: HashMap<String, String>,
    /// Parents on this page that claim replies. `conversations.history` never returns
    /// thread replies, so without fetching these the corpus holds openings without
    /// answers — in DMs and named channels alike, not only where we were mentioned.
    pub reply_parents: HashSet<String>,
    /// Messages on this page whose stored text differed from what Slack just returned.
    pub revised: Vec<String>,
}

/// Classify a page and commit the survivors in one transaction.
///
/// The conversation is upserted first inside the same transaction, so a conversation
/// discovered mid-page cannot produce a foreign-key violation on its own messages.
/// Messages that no rule keeps are dropped here and never written anywhere.
pub async fn write_page(
    pool: &PgPool,
    conversation: &mut Conversation,
    messages: &[Message],
    context: &IngestContext,
) -> Result<PageResult, IngestError> {
    let mut result = PageResult::default();
    let mut became_in_scope = false;

    {
        // The guard is held across the whole classify loop and no further: classifying
        // does no I/O, and nothing may await while it is held.
        let known = context.known_threads();
        for message in messages {
            let decision = classify(
                message,
                conversation,
                &context.my_user_id,
                &context.scope_channels,
                &known,
            );
            if !decision.stores() {
                continue;
            }
            result.stored.push(message.clone());
            if message.reply_count().unwrap_or(0) > 0 {
                result.reply_parents.insert(message.ts.clone());
            }
            match (decision.action, decision.thread_key) {
                (Action::StoreAndRegisterThread, Some(key)) => {
                    result.new_threads.insert(key.clone());
                    result
                        .mention_of
                        .entry(key)
                        .or_insert_with(|| message.ts.clone());
                    became_in_scope = true;
                }
                _ if decision.reason == Some(ScopeReason::MentionThread) => {
                    if let Some(thread_ts) = message.thread_ts.clone() {
                        result.touched_threads.insert(thread_ts);
                    }
                }
                _ => {}
            }
        }
    }

    // Before the upsert, because the upsert is what would otherwise make the old text
    // unrecoverable. Every message here is one the archive already holds — a first
    // sighting returns false and costs nothing — so this is an observation of change,
    // not a guess about which loop is running.
    let timestamps: Vec<String> = result.stored.iter().map(|m| m.ts.clone()).collect();
    let held =
        store::stored_texts(pool, &context.account_id, &conversation.id, &timestamps).await?;
    for message in &result.stored {
        let Some(stored) = held.get(&message.ts) else {
            continue;
        };
        if store::text_differs(stored.as_deref(), Some(&message.text))
            && store::record_revision(
                pool,
                &context.account_id,
                &conversation.id,
                &message.ts,
                Some(&message.text),
            )
            .await?
        {
            result.revised.push(message.ts.clone());
        }
    }

    let mut tx = pool.begin().await?;
    if became_in_scope && !conversation.in_scope {
        // A mention is what brings an otherwise-uninteresting channel into scope, and it
        // can only be known once a message has been read.
        conversation.in_scope = true;
        conversation.scope_reason = Some(ScopeReason::MentionThread);
    }
    store::upsert_conversation(&mut *tx, conversation).await?;
    store::upsert_messages(&mut tx, &result.stored).await?;
    for message in &result.stored {
        // History carries reactions as a snapshot, so every page is also a reaction
        // observation for the messages on it.
        store::replace_reactions(
            &mut tx,
            &context.account_id,
            &conversation.id,
            &message.ts,
            &message.reactions,
        )
        .await?;
    }
    for thread_ts in sorted(&result.new_threads) {
        let mention_ts = result
            .mention_of
            .get(&thread_ts)
            .cloned()
            .unwrap_or_else(|| thread_ts.clone());
        store::register_mention_thread(
            &mut tx,
            &context.account_id,
            &conversation.id,
            &thread_ts,
            &mention_ts,
        )
        .await?;
    }
    tx.commit().await?;

    context.remember_threads(&conversation.id, &result.new_threads);
    Ok(result)
}

/// Store every message of each named thread.
///
/// A mention brings the whole thread into scope, not just the mentioning message:
/// storing only the ping would leave the thread with a hole where the context was.
pub async fn fetch_threads<C: Slack>(
    pool: &PgPool,
    client: &C,
    conversation: &Conversation,
    thread_keys: &HashSet<String>,
    context: &IngestContext,
    reserve: f64,
) -> Result<usize, IngestError> {
    let mut written = 0;
    for thread_ts in sorted(thread_keys) {
        let messages = client
            .conversations_replies(&conversation.id, &thread_ts, &context.account_id, reserve)
            .await?;
        if messages.is_empty() {
            // An empty answer is a failed or vanished fetch, not a thread whose every
            // message was deleted. Reconciling on it would tombstone the lot.
            continue;
        }

        let held =
            store::stored_thread(pool, &context.account_id, &conversation.id, &thread_ts).await?;
        for message in &messages {
            if let Some(stored) = held.get(&message.ts)
                && store::text_differs(stored.as_deref(), Some(&message.text))
            {
                store::record_revision(
                    pool,
                    &context.account_id,
                    &conversation.id,
                    &message.ts,
                    Some(&message.text),
                )
                .await?;
            }
        }

        let mut tx = pool.begin().await?;
        store::upsert_messages(&mut tx, &messages).await?;
        for message in &messages {
            store::replace_reactions(
                &mut tx,
                &context.account_id,
                &conversation.id,
                &message.ts,
                &message.reactions,
            )
            .await?;
        }
        tx.commit().await?;

        // The one channel that can see a reply. `conversations.history` never returns
        // them, so the refresh window excludes replies from its deletion candidates
        // entirely; this is where a deleted reply becomes observable, and it costs no
        // request that was not already being made.
        let seen: HashSet<String> = messages.iter().map(|m| m.ts.clone()).collect();
        let present: Vec<String> = sorted(&seen)
            .into_iter()
            .filter(|ts| held.contains_key(ts))
            .collect();
        let absent: Vec<String> = held
            .keys()
            .filter(|ts| !seen.contains(*ts))
            .cloned()
            .collect();
        store::mark_present(pool, &context.account_id, &conversation.id, &present).await?;
        store::mark_absent(
            pool,
            &context.account_id,
            &conversation.id,
            &sorted_vec(absent),
        )
        .await?;
        written += messages.len();
    }
    Ok(written)
}

/// A set in a stable order. Python iterated `sorted(...)` at each of these points, and a
/// HashSet's order would otherwise make a call sequence unassertable.
fn sorted(keys: &HashSet<String>) -> Vec<String> {
    sorted_vec(keys.iter().cloned().collect())
}

fn sorted_vec(mut keys: Vec<String>) -> Vec<String> {
    keys.sort_unstable();
    keys
}
