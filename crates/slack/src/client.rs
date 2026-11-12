//! The Slack Web API over a browser session credential.
//!
//! Session credentials are not bearer tokens: the `xoxc` token goes in the form body
//! and the `xoxd` value goes in a `d` cookie. Slack answers a malformed cookie with
//! `not_authed` rather than anything naming the cookie, so the encoding rule below is
//! load-bearing and not cosmetic.
//!
//! This module knows nothing about Postgres, scope rules or MCP. It fetches and parses.

use std::time::Duration;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde_json::Value;
use slack_core::config::Secret;
use slack_core::models::{Conversation, Emoji, Message, Workspace, WorkspaceUser};
use tracing::warn;

use crate::errors::SlackError;
use crate::parse;
use crate::ratelimit::TokenBucket;

pub const BASE_URL: &str = "https://slack.com/api";

/// A session credential belongs to a browser, so the request should look like one.
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                                      AppleWebKit/537.36 (KHTML, like Gecko) \
                                      Chrome/139.0.0.0 Safari/537.36";

const MAX_ATTEMPTS: u32 = 5;
/// A rate limit is a wait, not a failure, so it does not consume a retry attempt — but
/// it needs its own ceiling, or a server answering 429 forever would loop forever.
const MAX_RATE_LIMIT_WAITS: u32 = 10;
/// Slack omits Retry-After often enough that this guess is load-bearing. Its Tier-3
/// window is a minute; half of it is long enough to clear a burst and short enough that
/// a wrong guess costs one extra 429 rather than a lost poll pass.
const DEFAULT_RETRY_AFTER: f64 = 30.0;

/// Slack's own page size, and the only one any caller asks for.
const PAGE_LIMIT: u32 = 200;

/// Everything `urllib.parse.quote(value, safe="")` leaves alone: the unreserved set.
/// `NON_ALPHANUMERIC` alone would also encode the `-` every `xoxd-` value starts with.
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'_')
    .remove(b'.')
    .remove(b'-')
    .remove(b'~');

/// One form field. `None` is dropped rather than sent: Slack reads an empty `latest` as
/// a bound, and an absent one as no bound.
type Params<'a> = Vec<(&'a str, Option<String>)>;

/// Percent-encode the `d` cookie value unless it already is.
///
/// Slack stores it encoded. Round-tripping an already-encoded value would turn `%2F`
/// into `%252F`, so the test is whether unquoting changes it.
pub fn encode_cookie_value(xoxd: &str) -> String {
    if percent_decode_str(xoxd).decode_utf8_lossy() != xoxd {
        return xoxd.to_owned();
    }
    utf8_percent_encode(xoxd, UNRESERVED).to_string()
}

/// One history request's bounds.
///
/// `oldest` walks forward (the poll) and `latest` walks backward (the backfill). The two
/// are one keystroke apart and swapping them is silent — it re-crawls history or loses
/// it — so every call site names them.
#[derive(Debug, Clone, Default)]
pub struct HistoryQuery<'a> {
    pub oldest: Option<&'a str>,
    pub latest: Option<&'a str>,
    pub cursor: Option<&'a str>,
    /// Page size, or 0 for Slack's own — so a caller that fills the rest of this struct
    /// from `Default` does not silently ask for a page of nothing.
    pub limit: u32,
    /// Marks the call as background work. See [`TokenBucket::acquire`].
    pub reserve: f64,
}

/// How the client waits between attempts.
///
/// `Real` is production. The tests use `Recorded`, which returns at once and keeps the
/// intervals, so a retry schedule can be asserted without the suite sleeping through it
/// — the seam the Python client spelled as a `sleep=` argument.
#[derive(Clone)]
enum Sleeper {
    Real,
    #[cfg(test)]
    Recorded(std::sync::Arc<std::sync::Mutex<Vec<f64>>>),
}

impl Sleeper {
    async fn sleep(&self, seconds: f64) {
        match self {
            Self::Real => tokio::time::sleep(Duration::from_secs_f64(seconds)).await,
            #[cfg(test)]
            Self::Recorded(log) => log.lock().expect("the sleep log").push(seconds),
        }
    }
}

/// One HTTP client and one rate limiter for one Slack account.
pub struct SlackClient {
    token: Secret,
    base_url: String,
    http: reqwest::Client,
    bucket: TokenBucket,
    sleeper: Sleeper,
}

impl SlackClient {
    pub fn new(xoxc: &str, xoxd: &str, rate: f64, burst: u32) -> Result<Self, SlackError> {
        Self::build(xoxc, xoxd, rate, burst, BASE_URL.to_owned(), Sleeper::Real)
    }

    fn build(
        xoxc: &str,
        xoxd: &str,
        rate: f64,
        burst: u32,
        base_url: String,
        sleeper: Sleeper,
    ) -> Result<Self, SlackError> {
        let mut headers = reqwest::header::HeaderMap::new();
        let cookie = format!("d={}", encode_cookie_value(xoxd));
        // The cookie carries the credential, so a malformed one must not be printed.
        let mut cookie = reqwest::header::HeaderValue::from_str(&cookie)
            .map_err(|_| SlackError::Config("the xoxd value is not a usable cookie".to_owned()))?;
        cookie.set_sensitive(true);
        headers.insert(reqwest::header::COOKIE, cookie);
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static(DEFAULT_USER_AGENT),
        );
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .default_headers(headers)
            .build()
            .map_err(|e| SlackError::Config(format!("no usable HTTP client: {e}")))?;
        Ok(Self {
            token: Secret::new(xoxc.to_owned()),
            base_url: base_url.trim_end_matches('/').to_owned(),
            http,
            bucket: TokenBucket::new(rate, burst)?,
            sleeper,
        })
    }

    // ------------------------------------------------------------------ egress point

    /// Every request in the project goes through here.
    ///
    /// Retries transport failures, 5xx and rate limits; never retries an auth failure,
    /// because a dead session does not recover on its own and each attempt is another
    /// request against a credential Slack has already rejected.
    ///
    /// `reserve` marks the call as background work — see [`TokenBucket::acquire`].
    pub async fn call(
        &self,
        method: &str,
        params: &[(&str, Option<String>)],
        reserve: f64,
    ) -> Result<Value, SlackError> {
        let mut form: Vec<(&str, &str)> = params
            .iter()
            .filter_map(|(key, value)| value.as_deref().map(|value| (*key, value)))
            .collect();
        form.push(("token", self.token.expose()));
        let url = format!("{}/{method}", self.base_url);

        let mut last_error = String::new();
        let mut rate_limit_waits = 0;
        let mut attempt = 0;

        while attempt < MAX_ATTEMPTS {
            attempt += 1;
            self.bucket.acquire(reserve).await;
            let response = match self.http.post(&url).form(&form).send().await {
                Ok(response) => response,
                Err(error) => {
                    last_error = format!("{method}: transport error: {error}");
                    self.backoff(attempt, &last_error).await;
                    continue;
                }
            };

            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);

            if status == 429 {
                // Waiting out a rate limit is the request working as intended, so it
                // gives the attempt back rather than spending it. Five slow calls would
                // otherwise look identical to five broken ones.
                attempt -= 1;
                rate_limit_waits = self
                    .wait_out_rate_limit(method, retry_after.as_deref(), rate_limit_waits)
                    .await?;
                continue;
            }
            if status >= 500 {
                last_error = format!("{method}: HTTP {status}");
                self.backoff(attempt, &last_error).await;
                continue;
            }

            // httpx reads the body inside `post`, so a body that stops mid-flight is a
            // transport failure there as well, and retried for the same reason.
            let body = match response.bytes().await {
                Ok(body) => body,
                Err(error) => {
                    last_error = format!("{method}: truncated response: {error}");
                    self.backoff(attempt, &last_error).await;
                    continue;
                }
            };
            let payload: Value = serde_json::from_slice(&body).map_err(|error| {
                SlackError::Transport(format!(
                    "{method}: HTTP {status}: unreadable payload: {error}"
                ))
            })?;
            if payload.get("ok").and_then(Value::as_bool) == Some(true) {
                return Ok(payload);
            }

            let code = payload
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error");
            if code == "ratelimited" {
                attempt -= 1;
                rate_limit_waits = self
                    .wait_out_rate_limit(method, retry_after.as_deref(), rate_limit_waits)
                    .await?;
                continue;
            }
            return Err(SlackError::api(method, code));
        }

        Err(SlackError::Transport(format!(
            "{method}: giving up after {MAX_ATTEMPTS} attempts: {last_error}"
        )))
    }

    /// Sleep off one rate limit and return the new wait count.
    ///
    /// Bounded separately from the retry budget: giving the attempt back is right, but a
    /// server answering 429 forever would otherwise loop forever.
    async fn wait_out_rate_limit(
        &self,
        method: &str,
        retry_after: Option<&str>,
        waits: u32,
    ) -> Result<u32, SlackError> {
        let waits = waits + 1;
        if waits > MAX_RATE_LIMIT_WAITS {
            return Err(SlackError::Transport(format!(
                "{method}: rate limited {waits} times in a row"
            )));
        }
        // Draining matters: without it every concurrent caller has to hit its own 429 to
        // learn the same fact, which is several more requests into a limit we are
        // already over.
        let delay = retry_after
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|delay| delay.is_finite() && *delay >= 0.0)
            .unwrap_or(DEFAULT_RETRY_AFTER);
        warn!("{method} rate limited, sleeping {delay:.1}s");
        self.bucket.drain().await;
        self.sleeper.sleep(delay).await;
        Ok(waits)
    }

    async fn backoff(&self, attempt: u32, reason: &str) {
        if attempt >= MAX_ATTEMPTS {
            // The Python slept here too, which delayed the error by sixteen seconds and
            // bought nothing: there is no attempt left to wait for.
            warn!("{reason}, out of attempts ({attempt}/{MAX_ATTEMPTS})");
            return;
        }
        let delay = 2f64.powi(attempt as i32 - 1);
        warn!("{reason}, retrying in {delay:.0}s (attempt {attempt}/{MAX_ATTEMPTS})");
        self.sleeper.sleep(delay).await;
    }

    // -------------------------------------------------------------------- pagination

    /// Walk every page of a cursor-paginated method, mapping each item as it arrives.
    ///
    /// The cursor is the loop condition, not a page count or an empty page: Slack ends a
    /// listing with a page that carries a present-but-empty `next_cursor`, and a listing
    /// that never paginated omits `response_metadata` entirely. Testing the wrong field
    /// silently truncates history.
    async fn paginate<T>(
        &self,
        method: &str,
        key: &str,
        extra: Params<'_>,
        reserve: f64,
        mut item: impl FnMut(&Value) -> Result<T, SlackError>,
    ) -> Result<Vec<T>, SlackError> {
        let mut collected = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut params = extra.clone();
            params.push(("cursor", cursor));
            params.push(("limit", Some(PAGE_LIMIT.to_string())));
            let payload = self.call(method, &params, reserve).await?;
            if let Some(page) = payload.get(key).and_then(Value::as_array) {
                for raw in page {
                    collected.push(item(raw)?);
                }
            }
            cursor = next_cursor(&payload);
            if cursor.is_none() {
                return Ok(collected);
            }
        }
    }

    // ------------------------------------------------------------- typed operations

    /// `(team_id, user_id)` for the credential in hand.
    pub async fn auth_test(&self) -> Result<(String, String), SlackError> {
        let payload = self.call("auth.test", &[], 0.0).await?;
        let field = |name: &str| {
            payload
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| SlackError::Transport(format!("auth.test: no {name}")))
        };
        Ok((field("team_id")?, field("user_id")?))
    }

    pub async fn team_info(&self, team: Option<&str>) -> Result<Workspace, SlackError> {
        let payload = self
            .call("team.info", &[("team", team.map(str::to_owned))], 0.0)
            .await?;
        parse::parse_workspace(&payload["team"], team.is_some())
    }

    /// Every conversation this user is a member of.
    ///
    /// `users.conversations`, not `conversations.list`: the latter lists the workspace's
    /// channels rather than mine, and only returns DMs when asked with the right types —
    /// this one is the direct answer to "where am I".
    pub async fn users_conversations(
        &self,
        account_id: &str,
        default_team_id: Option<&str>,
    ) -> Result<Vec<Conversation>, SlackError> {
        let params = vec![
            (
                "types",
                Some("public_channel,private_channel,mpim,im".to_owned()),
            ),
            ("exclude_archived", Some("true".to_owned())),
        ];
        self.paginate("users.conversations", "channels", params, 0.0, |raw| {
            parse::parse_conversation(raw, account_id, default_team_id)
        })
        .await
    }

    pub async fn conversations_info(
        &self,
        channel: &str,
        account_id: &str,
        default_team_id: Option<&str>,
    ) -> Result<Conversation, SlackError> {
        let payload = self
            .call(
                "conversations.info",
                &[("channel", Some(channel.to_owned()))],
                0.0,
            )
            .await?;
        parse::parse_conversation(&payload["channel"], account_id, default_team_id)
    }

    /// One page of history, plus the cursor for the next.
    ///
    /// Returns the cursor rather than looping internally: the ingester persists it
    /// between passes so an interrupted backfill resumes where it stopped instead of
    /// restarting from the top of the channel.
    pub async fn conversations_history(
        &self,
        channel: &str,
        account_id: &str,
        query: HistoryQuery<'_>,
    ) -> Result<(Vec<Message>, Option<String>), SlackError> {
        let own = |value: Option<&str>| value.map(str::to_owned);
        let params = [
            ("channel", Some(channel.to_owned())),
            ("oldest", own(query.oldest)),
            ("latest", own(query.latest)),
            ("cursor", own(query.cursor)),
            (
                "limit",
                Some(
                    if query.limit == 0 {
                        PAGE_LIMIT
                    } else {
                        query.limit
                    }
                    .to_string(),
                ),
            ),
        ];
        let payload = self
            .call("conversations.history", &params, query.reserve)
            .await?;
        let mut messages = Vec::new();
        if let Some(page) = payload.get("messages").and_then(Value::as_array) {
            for raw in page {
                messages.push(parse::parse_message(raw, account_id, channel)?);
            }
        }
        Ok((messages, next_cursor(&payload)))
    }

    /// A whole thread, parent included, following the cursor to the end.
    pub async fn conversations_replies(
        &self,
        channel: &str,
        thread_ts: &str,
        account_id: &str,
        reserve: f64,
    ) -> Result<Vec<Message>, SlackError> {
        let params = vec![
            ("channel", Some(channel.to_owned())),
            ("ts", Some(thread_ts.to_owned())),
        ];
        self.paginate(
            "conversations.replies",
            "messages",
            params,
            reserve,
            |raw| parse::parse_message(raw, account_id, channel),
        )
        .await
    }

    pub async fn users_list(&self, account_id: &str) -> Result<Vec<WorkspaceUser>, SlackError> {
        self.paginate("users.list", "members", Vec::new(), 0.0, |raw| {
            parse::parse_user(raw, account_id)
        })
        .await
    }

    pub async fn emoji_list(&self, team_id: &str) -> Result<Vec<Emoji>, SlackError> {
        let payload = self.call("emoji.list", &[], 0.0).await?;
        Ok(parse::parse_emoji(&payload["emoji"], team_id))
    }
}

/// Slack ends a listing with a present-but-empty `next_cursor`, so an empty string is
/// the end and not a cursor.
fn next_cursor(payload: &Value) -> Option<String> {
    payload
        .get("response_metadata")?
        .get("next_cursor")?
        .as_str()
        .filter(|cursor| !cursor.is_empty())
        .map(str::to_owned)
}

/// The client driven through a fake Slack on loopback. No test here touches Slack.
///
/// The fake is a plain TCP listener rather than an HTTP server crate: it has to answer
/// with exact headers, and it has to be able to accept a request and then hang up
/// without answering at all, which is how a transport failure mid-request is reproduced.
#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;
    use crate::fixtures::load;

    #[derive(Clone)]
    enum Reply {
        /// A complete response: this status, this JSON body, and `Retry-After` when set.
        Body {
            status: u16,
            retry_after: Option<&'static str>,
            json: String,
        },
        /// Read the request and close the connection without answering.
        Hangup,
    }

    fn ok(fixture: &str) -> Reply {
        Reply::Body {
            status: 200,
            retry_after: None,
            json: load(fixture).to_string(),
        }
    }

    fn json(status: u16, body: Value) -> Reply {
        Reply::Body {
            status,
            retry_after: None,
            json: body.to_string(),
        }
    }

    fn status(code: u16) -> Reply {
        Reply::Body {
            status: code,
            retry_after: None,
            json: String::new(),
        }
    }

    fn rate_limited(retry_after: Option<&'static str>) -> Reply {
        Reply::Body {
            status: 429,
            retry_after,
            json: String::new(),
        }
    }

    struct Recorded {
        head: String,
        body: String,
    }

    /// A scripted Slack. `replies` are served in order; `forever` serves the same reply
    /// to every request, for the cases that assert a retry never stops.
    struct Fake {
        addr: SocketAddr,
        replies: Mutex<VecDeque<Reply>>,
        forever: Option<Reply>,
        requests: Mutex<Vec<Recorded>>,
    }

    impl Fake {
        async fn start(replies: Vec<Reply>, forever: Option<Reply>) -> Arc<Self> {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let fake = Arc::new(Self {
                addr: listener.local_addr().unwrap(),
                replies: Mutex::new(replies.into()),
                forever,
                requests: Mutex::new(Vec::new()),
            });
            let serving = Arc::clone(&fake);
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    serving.serve_one(stream).await;
                }
            });
            fake
        }

        /// One request per connection: every answer carries `Connection: close`, so the
        /// requests arrive in the order the client made them and nothing has to parse a
        /// pipeline.
        async fn serve_one(&self, mut stream: TcpStream) {
            let mut buffer = Vec::new();
            let head_end = loop {
                let mut chunk = [0u8; 4096];
                let read = stream.read(&mut chunk).await.unwrap_or(0);
                if read == 0 {
                    return;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at + 4;
                }
            };
            let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
            let length: usize = head
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length: ")
                        .or_else(|| line.strip_prefix("Content-Length: "))
                })
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            while buffer.len() < head_end + length {
                let mut chunk = [0u8; 4096];
                let read = stream.read(&mut chunk).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            let body = String::from_utf8_lossy(&buffer[head_end..]).to_string();
            self.requests
                .lock()
                .expect("the request log")
                .push(Recorded { head, body });

            let reply = self
                .replies
                .lock()
                .expect("the reply script")
                .pop_front()
                .or_else(|| self.forever.clone())
                .expect("the client made more requests than the test scripted");
            let Reply::Body {
                status,
                retry_after,
                json,
            } = reply
            else {
                return; // Hangup: close without answering.
            };
            let header = retry_after
                .map(|value| format!("Retry-After: {value}\r\n"))
                .unwrap_or_default();
            let response = format!(
                "HTTP/1.1 {status} X\r\n{header}Content-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{json}",
                json.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }

        fn count(&self) -> usize {
            self.requests.lock().expect("the request log").len()
        }

        /// The form body of one request, decoded.
        fn body(&self, index: usize) -> HashMap<String, String> {
            let requests = self.requests.lock().expect("the request log");
            requests[index]
                .body
                .split('&')
                .filter(|pair| !pair.is_empty())
                .map(|pair| {
                    let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                    let decode = |raw: &str| {
                        percent_decode_str(&raw.replace('+', " "))
                            .decode_utf8_lossy()
                            .to_string()
                    };
                    (decode(key), decode(value))
                })
                .collect()
        }

        /// One request header, lowercased name, as sent.
        fn header(&self, index: usize, name: &str) -> Option<String> {
            let requests = self.requests.lock().expect("the request log");
            requests[index].head.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                (key.trim().eq_ignore_ascii_case(name)).then(|| value.trim().to_owned())
            })
        }
    }

    /// A client pointed at the fake, whose sleeps are recorded rather than slept. The
    /// rate is far above anything a test asks for, so the bucket never adds a wait of
    /// its own.
    async fn client(
        replies: Vec<Reply>,
        forever: Option<Reply>,
    ) -> (SlackClient, Arc<Fake>, Arc<Mutex<Vec<f64>>>) {
        let fake = Fake::start(replies, forever).await;
        let slept = Arc::new(Mutex::new(Vec::new()));
        let client = SlackClient::build(
            "xoxc-secret",
            "xoxd-secret",
            1000.0,
            1000,
            format!("http://{}/api", fake.addr),
            Sleeper::Recorded(Arc::clone(&slept)),
        )
        .unwrap();
        (client, fake, slept)
    }

    fn slept(log: &Arc<Mutex<Vec<f64>>>) -> Vec<f64> {
        log.lock().expect("the sleep log").clone()
    }

    // ---------------------------------------------------------------------- auth

    #[test]
    fn cookie_encoding_is_idempotent() {
        // Slack stores `d` percent-encoded; re-encoding it would turn %2F into %252F.
        // Either mistake fails as `not_authed`, which names nothing, so the rule is
        // pinned here rather than discovered in production.
        for (raw, expected) in [
            ("xoxd-abc%2Fdef", "xoxd-abc%2Fdef"),
            ("xoxd-abc/def", "xoxd-abc%2Fdef"),
            ("xoxd-plain", "xoxd-plain"),
        ] {
            assert_eq!(encode_cookie_value(raw), expected, "{raw}");
        }
    }

    #[tokio::test]
    async fn credential_travels_in_the_body_and_the_cookie() {
        // Not an Authorization header: a session token is a form field.
        let (client, fake, _) = client(vec![ok("auth_test")], None).await;
        assert_eq!(
            client.auth_test().await.unwrap(),
            ("T0001".to_owned(), "U0ME".to_owned())
        );
        assert_eq!(fake.body(0).get("token").unwrap(), "xoxc-secret");
        assert_eq!(fake.header(0, "cookie").unwrap(), "d=xoxd-secret");
        assert_eq!(fake.header(0, "authorization"), None);
        assert_eq!(
            fake.header(0, "content-type").unwrap(),
            "application/x-www-form-urlencoded"
        );
        assert!(fake.header(0, "user-agent").unwrap().contains("Chrome"));
    }

    #[tokio::test]
    async fn auth_failure_is_raised_without_a_retry() {
        // A dead session does not recover; retrying just spends requests on it.
        let (client, fake, _) = client(Vec::new(), Some(ok("error_invalid_auth"))).await;
        let error = client.auth_test().await.unwrap_err();
        assert!(error.is_auth());
        assert_eq!(error.code(), Some("invalid_auth"));
        assert_eq!(fake.count(), 1);
    }

    #[tokio::test]
    async fn other_api_errors_raise_without_a_retry() {
        let reply = json(
            200,
            serde_json::json!({"ok": false, "error": "channel_not_found"}),
        );
        let (client, fake, _) = client(Vec::new(), Some(reply)).await;
        let error = client
            .conversations_info("C0NOPE", "work", None)
            .await
            .unwrap_err();
        assert!(!error.is_auth());
        assert_eq!(error.code(), Some("channel_not_found"));
        assert_eq!(fake.count(), 1);
    }

    // ------------------------------------------------------------------- retries

    #[tokio::test]
    async fn server_errors_back_off_and_then_succeed() {
        let (client, fake, log) =
            client(vec![status(500), status(503), ok("auth_test")], None).await;
        client.auth_test().await.unwrap();
        assert_eq!(fake.count(), 3);
        assert_eq!(slept(&log), [1.0, 2.0]);
    }

    #[tokio::test]
    async fn transport_errors_are_retried() {
        let (client, fake, _) = client(vec![Reply::Hangup, ok("auth_test")], None).await;
        client.auth_test().await.unwrap();
        assert_eq!(fake.count(), 2);
    }

    #[tokio::test]
    async fn giving_up_raises_a_transport_error() {
        let (client, fake, log) = client(Vec::new(), Some(status(500))).await;
        let error = client.auth_test().await.unwrap_err();
        assert!(matches!(error, SlackError::Transport(_)), "{error}");
        assert_eq!(fake.count(), 5);
        // No sixteen-second sleep after the last attempt: there is nothing left to wait
        // for, and the Python client waited anyway.
        assert_eq!(slept(&log), [1.0, 2.0, 4.0, 8.0]);
    }

    #[tokio::test]
    async fn http_429_sleeps_for_retry_after_and_drains_the_bucket() {
        let (client, _fake, log) =
            client(vec![rate_limited(Some("2")), ok("auth_test")], None).await;
        client.auth_test().await.unwrap();
        assert_eq!(slept(&log), [2.0]);
        // Draining is what makes concurrent callers back off too, rather than each one
        // discovering the same 429 for itself.
        assert_eq!(client.bucket.drains(), 1);
    }

    #[tokio::test]
    async fn ok_false_ratelimited_is_treated_as_429() {
        let (client, fake, log) =
            client(vec![ok("error_ratelimited"), ok("auth_test")], None).await;
        client.auth_test().await.unwrap();
        assert_eq!(slept(&log), [DEFAULT_RETRY_AFTER]);
        assert_eq!(fake.count(), 2);
    }

    #[tokio::test]
    async fn unparseable_retry_after_falls_back_to_the_default() {
        let (client, _fake, log) =
            client(vec![rate_limited(Some("in a bit")), ok("auth_test")], None).await;
        client.auth_test().await.unwrap();
        assert_eq!(slept(&log), [DEFAULT_RETRY_AFTER]);
    }

    #[tokio::test]
    async fn rate_limits_do_not_consume_the_retry_budget() {
        // A rate limit is a wait, not a failure. Five slow calls are not five broken
        // ones.
        let mut replies = vec![rate_limited(Some("1")); 7];
        replies.push(ok("auth_test"));
        let (client, fake, log) = client(replies, None).await;
        client.auth_test().await.unwrap();
        assert_eq!(fake.count(), 8);
        assert_eq!(slept(&log), [1.0; 7]);
    }

    #[tokio::test]
    async fn an_endless_rate_limit_still_gives_up() {
        // Giving the attempt back needs its own ceiling or the call never returns.
        let (client, fake, _) = client(Vec::new(), Some(rate_limited(Some("1")))).await;
        let error = client.auth_test().await.unwrap_err();
        assert!(error.to_string().contains("rate limited"), "{error}");
        assert_eq!(fake.count(), 11);
    }

    #[tokio::test]
    async fn a_5xx_after_rate_limits_still_has_its_full_budget() {
        let mut replies = vec![rate_limited(Some("1"))];
        replies.extend(vec![status(500); 4]);
        replies.push(ok("auth_test"));
        let (client, fake, _) = client(replies, None).await;
        client.auth_test().await.unwrap();
        assert_eq!(fake.count(), 6);
    }

    // ---------------------------------------------------------------- pagination

    #[tokio::test]
    async fn pagination_follows_the_cursor_to_an_empty_one() {
        // The cursor is the loop condition. The last page is non-empty and still final.
        let (client, fake, _) = client(
            vec![
                ok("users_conversations_page1"),
                ok("users_conversations_page2"),
            ],
            None,
        )
        .await;
        let conversations = client.users_conversations("work", None).await.unwrap();
        assert_eq!(
            conversations
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            ["C0GENERAL", "D0TED", "G0PLANS", "C0PARTNER"]
        );
        assert_eq!(fake.count(), 2);
        assert!(!fake.body(0).contains_key("cursor"));
        assert_eq!(fake.body(1).get("cursor").unwrap(), "dXNlcjpVMDYxTkZUVDI=");
    }

    #[tokio::test]
    async fn pagination_stops_when_response_metadata_is_absent() {
        let reply = json(200, serde_json::json!({"ok": true, "channels": []}));
        let (client, fake, _) = client(vec![reply], None).await;
        assert!(
            client
                .users_conversations("work", None)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(fake.count(), 1);
    }

    #[tokio::test]
    async fn form_booleans_are_slack_shaped() {
        let (client, fake, _) = client(vec![ok("users_conversations_page2")], None).await;
        client.users_conversations("work", None).await.unwrap();
        assert_eq!(fake.body(0).get("exclude_archived").unwrap(), "true");
    }

    // ---------------------------------------------------------- typed operations

    #[tokio::test]
    async fn history_returns_a_page_and_its_cursor() {
        // The cursor is handed back, not looped on: the ingester persists it so an
        // interrupted backfill resumes instead of restarting at the top of the channel.
        let (client, fake, _) = client(vec![ok("conversations_history")], None).await;
        let (messages, cursor) = client
            .conversations_history(
                "C0GENERAL",
                "work",
                HistoryQuery {
                    oldest: Some("1723600000.000000"),
                    ..HistoryQuery::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            messages.iter().map(|m| m.ts.as_str()).collect::<Vec<_>>(),
            [
                "1723640400.001200",
                "1723640460.000100",
                "1723640600.000200",
                "1723640700.000300",
            ]
        );
        assert_eq!(cursor.as_deref(), Some("bmV4dF90czoxNzIzNjQwNzAw"));
        assert_eq!(fake.body(0).get("oldest").unwrap(), "1723600000.000000");
        assert!(!fake.body(0).contains_key("latest"));
    }

    #[tokio::test]
    async fn history_final_page_reports_no_cursor() {
        let (client, _fake, _) = client(vec![ok("conversations_history_last")], None).await;
        let (_messages, cursor) = client
            .conversations_history("C0GENERAL", "work", HistoryQuery::default())
            .await
            .unwrap();
        assert_eq!(cursor, None);
    }

    #[tokio::test]
    async fn replies_return_the_whole_thread() {
        let (client, fake, _) = client(vec![ok("conversations_replies")], None).await;
        let thread = client
            .conversations_replies("C0GENERAL", "1723640400.001200", "work", 0.0)
            .await
            .unwrap();
        assert_eq!(
            thread.iter().map(|m| m.ts.as_str()).collect::<Vec<_>>(),
            ["1723640400.001200", "1723640460.000100"]
        );
        assert_eq!(fake.body(0).get("ts").unwrap(), "1723640400.001200");
    }

    #[tokio::test]
    async fn team_info_marks_a_foreign_team_external() {
        // `team.info` with no argument describes my own team; with one it describes
        // theirs.
        let (client, _fake, _) = client(vec![ok("team_info"), ok("team_info")], None).await;
        let mine = client.team_info(None).await.unwrap();
        let theirs = client.team_info(Some("T0EXT")).await.unwrap();
        assert!(!mine.is_external);
        assert!(theirs.is_external);
        assert_eq!(mine.name.as_deref(), Some("Acme"));
    }

    #[tokio::test]
    async fn users_and_emoji_round_trip() {
        let (client, _fake, _) = client(vec![ok("users_list"), ok("emoji_list")], None).await;
        let users = client.users_list("work").await.unwrap();
        let emojis = client.emoji_list("T0001").await.unwrap();
        assert_eq!(
            users.iter().map(|u| u.user_id.as_str()).collect::<Vec<_>>(),
            ["U0TED", "B0BOT"]
        );
        let mut names: Vec<&str> = emojis.iter().map(|e| e.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["shipit", "yolo"]);
    }

    #[tokio::test]
    async fn background_calls_pass_a_reserve_and_never_reach_the_request_body() {
        let (client, fake, _) = client(vec![ok("conversations_history")], None).await;
        client
            .conversations_history(
                "C0GENERAL",
                "work",
                HistoryQuery {
                    reserve: 2.0,
                    ..HistoryQuery::default()
                },
            )
            .await
            .unwrap();
        assert!(!fake.body(0).contains_key("reserve"));
        // ...and it did reach the bucket, which is the only thing that can act on it.
        // The backfill passing `POLL_RESERVE` is asserted in `tests/archive/loops.rs`,
        // and what the bucket then does with it in `ratelimit`'s own tests; this is the
        // link between the two.
        assert_eq!(client.bucket.reserves(), [2.0]);
    }

    #[tokio::test]
    async fn a_payload_that_is_not_json_is_reported_rather_than_retried() {
        // The Python raised a bare JSONDecodeError here, which named neither the method
        // nor the status.
        let reply = Reply::Body {
            status: 200,
            retry_after: None,
            json: "<html>maintenance</html>".to_owned(),
        };
        let (client, fake, _) = client(vec![reply], None).await;
        let error = client.auth_test().await.unwrap_err();
        assert!(error.to_string().contains("unreadable payload"), "{error}");
        assert_eq!(fake.count(), 1);
    }
}
