//! Typed configuration, one struct per process.
//!
//! Two settings types, because the two processes must not share a credential surface:
//! `IngestSettings` holds the Slack session credential, `McpSettings` deliberately has
//! no `xoxc`/`xoxd` field at all. That is the enforcement point for the whole read-only
//! design, and it is a compile-time fact here rather than a test over
//! `model_fields`.

use std::env;
use std::fmt;

/// A value that must not reach a log line. `Debug` prints the redaction, so a
/// `{settings:?}` anywhere cannot leak the DSN's password — the same guarantee
/// `pydantic.SecretStr` gives the Python side.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(**********)")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} is required")]
    Missing(&'static str),
    #[error("{name} is not a number: {value:?}")]
    NotANumber { name: &'static str, value: String },
    #[error("{name} is not a boolean: {value:?}")]
    NotABoolean { name: &'static str, value: String },
    #[error("port must be between 1 and 65535")]
    PortOutOfRange,
}

/// rmcp's own default, repeated here so the allowlist is never empty.
///
/// An empty list means "allow every `Host`" to rmcp, so the parse below falls back to
/// this rather than to nothing: a compose file that forgets `SLACK_MCP_ALLOWED_HOSTS`
/// should fail closed and be obvious, not fail open and be invisible. The consequence
/// is that there is deliberately no way to spell "allow all" through the environment.
pub const DEFAULT_ALLOWED_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// `SLACK_SCOPE_CHANNELS` split into names, `#` and whitespace stripped.
///
/// Split out from the environment read so it can be tested without `set_var`.
pub fn parse_scope_channels(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| name.trim_start_matches('#').to_owned())
        .collect()
}

/// Configuration for the ingester process.
///
/// The credential is three `Secret`s, so no `{settings:?}` anywhere can print a token —
/// the guarantee `pydantic.SecretStr` gives the Python side, and the reason the ingester
/// may log its own configuration at boot.
#[derive(Debug, Clone)]
pub struct IngestSettings {
    pub account: String,
    pub xoxc: Secret,
    pub xoxd: Secret,
    pub dsn: Secret,
    /// Channel names that are in scope by name, already split and stripped.
    pub scope_channels: Vec<String>,
    /// The sleep between passes, not the cycle: a pass itself takes a minute or two.
    pub poll_interval: u64,
    /// A conversation with nothing newer than this is read only every
    /// `poll_dormant_every`-th pass, so a pass costs what the active ones cost.
    pub poll_dormant_days: i64,
    pub poll_dormant_every: u32,
    pub reaction_window: i64,
    /// How far back the first poll pass reads. Deep history is backfill's job; without
    /// this the first pass crawls every conversation to its beginning.
    pub poll_bootstrap_days: i64,
    /// Deliberately below Slack's Tier-3 "50+/min".
    pub rate_limit_rps: f64,
    pub rate_limit_burst: u32,
    pub backfill_enabled: bool,
    pub log_level: String,
}

impl IngestSettings {
    /// Read `SLACK_*` from the environment, with the Python defaults.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_source(|name| env::var(name).ok())
    }

    /// The same read, against any source of variables.
    ///
    /// `from_env` is one line over this so the variable *names* are testable: `set_var`
    /// is process-global and unsafe in edition 2024, so a test that set them would race
    /// every other test in the binary.
    pub fn from_source(get: impl Fn(&'static str) -> Option<String>) -> Result<Self, ConfigError> {
        let required = |name: &'static str| get(name).ok_or(ConfigError::Missing(name));
        let number = |name: &'static str, default: f64| -> Result<f64, ConfigError> {
            match get(name) {
                Some(raw) => raw.trim().parse().map_err(|_| ConfigError::NotANumber {
                    name,
                    value: raw.clone(),
                }),
                None => Ok(default),
            }
        };
        let flag = |name: &'static str, default: bool| -> Result<bool, ConfigError> {
            match get(name) {
                // pydantic's set, so a compose file that says `0` or `no` keeps meaning
                // what it meant. Anything else is a typo, and is reported rather than
                // silently read as false.
                Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                    "1" | "true" | "yes" | "on" => Ok(true),
                    "0" | "false" | "no" | "off" => Ok(false),
                    _ => Err(ConfigError::NotABoolean { name, value: raw }),
                },
                None => Ok(default),
            }
        };
        Ok(Self {
            account: required("SLACK_ACCOUNT")?,
            xoxc: Secret(required("SLACK_XOXC")?),
            xoxd: Secret(required("SLACK_XOXD")?),
            dsn: Secret(required("SLACK_DSN")?),
            scope_channels: parse_scope_channels(&get("SLACK_SCOPE_CHANNELS").unwrap_or_default()),
            poll_interval: number("SLACK_POLL_INTERVAL", 120.0)? as u64,
            poll_dormant_days: number("SLACK_POLL_DORMANT_DAYS", 30.0)? as i64,
            poll_dormant_every: number("SLACK_POLL_DORMANT_EVERY", 5.0)? as u32,
            reaction_window: number("SLACK_REACTION_WINDOW", 7.0)? as i64,
            poll_bootstrap_days: number("SLACK_POLL_BOOTSTRAP_DAYS", 2.0)? as i64,
            rate_limit_rps: number("SLACK_RATE_LIMIT_RPS", 0.8)?,
            rate_limit_burst: number("SLACK_RATE_LIMIT_BURST", 5.0)? as u32,
            backfill_enabled: flag("SLACK_BACKFILL_ENABLED", true)?,
            log_level: get("SLACK_LOG_LEVEL").unwrap_or_else(|| "INFO".to_owned()),
        })
    }
}

#[derive(Debug, Clone)]
pub struct McpSettings {
    pub dsn: Secret,
    pub host: String,
    pub port: u16,
    pub log_level: String,
    /// `Host` headers the MCP endpoint will answer, as rmcp's DNS-rebinding guard.
    ///
    /// An entry without a port matches that hostname on any port; an entry with one
    /// requires an exact match. MCPHub addresses this server by container name
    /// (`http://slack-mcp:8000/mcp`), so `slack-mcp` is the production value — written
    /// without a port on purpose, because `SLACK_MCP_PORT` is already configurable and
    /// pinning the port in two places invites the two to disagree.
    pub allowed_hosts: Vec<String>,
}

/// Where the server listens, read on its own so `--health-check` can find the port
/// without a DSN it has no use for.
pub fn address_from_env() -> Result<(String, u16), ConfigError> {
    let port = match env::var("SLACK_MCP_PORT") {
        Ok(raw) => {
            let parsed: u32 = raw.trim().parse().map_err(|_| ConfigError::NotANumber {
                name: "SLACK_MCP_PORT",
                value: raw.clone(),
            })?;
            if !(1..=65535).contains(&parsed) {
                return Err(ConfigError::PortOutOfRange);
            }
            parsed as u16
        }
        Err(_) => 8000,
    };
    Ok((
        env::var("SLACK_MCP_HOST").unwrap_or_else(|_| "0.0.0.0".to_owned()),
        port,
    ))
}

/// `SLACK_MCP_ALLOWED_HOSTS` as a comma-separated list, falling back to
/// [`DEFAULT_ALLOWED_HOSTS`].
///
/// Blank entries are dropped, and a value that is entirely blank or absent yields the
/// default rather than an empty list — see [`DEFAULT_ALLOWED_HOSTS`] for why that
/// matters.
pub fn allowed_hosts_from_env() -> Vec<String> {
    parse_allowed_hosts(&env::var("SLACK_MCP_ALLOWED_HOSTS").unwrap_or_default())
}

/// Split out from the environment read so it can be tested without `set_var`, which is
/// process-global and unsafe in edition 2024.
pub fn parse_allowed_hosts(raw: &str) -> Vec<String> {
    let configured: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_owned)
        .collect();
    if configured.is_empty() {
        return DEFAULT_ALLOWED_HOSTS
            .iter()
            .map(|host| (*host).to_owned())
            .collect();
    }
    configured
}

impl McpSettings {
    /// Read `SLACK_MCP_*` from the environment, with the Python defaults.
    pub fn from_env() -> Result<Self, ConfigError> {
        let (host, port) = address_from_env()?;
        Ok(Self {
            dsn: Secret(
                env::var("SLACK_MCP_DSN").map_err(|_| ConfigError::Missing("SLACK_MCP_DSN"))?,
            ),
            host,
            port,
            log_level: env::var("SLACK_MCP_LOG_LEVEL").unwrap_or_else(|_| "INFO".to_owned()),
            allowed_hosts: allowed_hosts_from_env(),
        })
    }

    pub fn for_dsn(dsn: impl Into<String>) -> Self {
        Self {
            dsn: Secret(dsn.into()),
            host: "0.0.0.0".to_owned(),
            port: 8000,
            log_level: "INFO".to_owned(),
            allowed_hosts: DEFAULT_ALLOWED_HOSTS
                .iter()
                .map(|h| (*h).to_owned())
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four required variables, plus whatever the case under test adds.
    fn ingest(vars: &[(&str, &str)]) -> IngestSettings {
        let mut source: Vec<(String, String)> = vec![
            ("SLACK_ACCOUNT", "work"),
            ("SLACK_XOXC", "xoxc-test"),
            ("SLACK_XOXD", "xoxd-test"),
            ("SLACK_DSN", "postgresql://u:p@localhost/slack"),
        ]
        .into_iter()
        .chain(vars.iter().copied())
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
        source.reverse(); // the case's own values win over the defaults above
        IngestSettings::from_source(|name| {
            source
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        })
        .expect("settings")
    }

    #[test]
    fn an_ingest_secret_does_not_print() {
        let settings = ingest(&[]);
        let rendered = format!("{settings:?}");
        assert!(!rendered.contains("xoxc-test"), "{rendered}");
        assert!(!rendered.contains("xoxd-test"), "{rendered}");
        assert_eq!(settings.xoxc.expose(), "xoxc-test");
    }

    #[test]
    fn scope_channel_names_strip_the_hash_and_the_whitespace() {
        assert_eq!(
            parse_scope_channels(" #general , general-dev ,, "),
            ["general", "general-dev"]
        );
    }

    #[test]
    fn scope_channel_names_are_empty_by_default() {
        assert!(ingest(&[]).scope_channels.is_empty());
    }

    #[test]
    fn ingest_settings_read_the_names_the_compose_file_sets() {
        // The point of `from_source`: a renamed variable fails here rather than in
        // production, where it would quietly read as a default.
        let settings = ingest(&[
            ("SLACK_SCOPE_CHANNELS", "#general"),
            ("SLACK_POLL_INTERVAL", "30"),
            ("SLACK_RATE_LIMIT_RPS", "0.5"),
            ("SLACK_BACKFILL_ENABLED", "0"),
            ("SLACK_REACTION_WINDOW", "3"),
        ]);
        assert_eq!(settings.account, "work");
        assert_eq!(settings.scope_channels, ["general"]);
        assert_eq!(settings.poll_interval, 30);
        assert_eq!(settings.rate_limit_rps, 0.5);
        assert!(!settings.backfill_enabled);
        assert_eq!(settings.reaction_window, 3);
        // The defaults survive beside the overrides.
        assert_eq!(settings.poll_dormant_days, 30);
        assert_eq!(settings.rate_limit_burst, 5);
    }

    #[test]
    fn a_missing_credential_is_an_error_not_an_empty_string() {
        let error = IngestSettings::from_source(|_| None).unwrap_err();
        assert!(error.to_string().contains("SLACK_ACCOUNT"), "{error}");
    }

    #[test]
    fn an_unreadable_number_or_flag_is_reported_not_defaulted() {
        let only = |extra: (&'static str, &'static str)| {
            move |name: &'static str| match name {
                "SLACK_ACCOUNT" | "SLACK_XOXC" | "SLACK_XOXD" | "SLACK_DSN" => Some("x".to_owned()),
                other if other == extra.0 => Some(extra.1.to_owned()),
                _ => None,
            }
        };
        assert!(matches!(
            IngestSettings::from_source(only(("SLACK_POLL_INTERVAL", "soon"))),
            Err(ConfigError::NotANumber { .. })
        ));
        assert!(matches!(
            IngestSettings::from_source(only(("SLACK_BACKFILL_ENABLED", "maybe"))),
            Err(ConfigError::NotABoolean { .. })
        ));
    }

    #[test]
    fn the_dsn_never_appears_in_a_debug_line() {
        let settings = McpSettings::for_dsn("postgresql://slack_mcp:hunter2@db/slack");
        let rendered = format!("{settings:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
    }

    #[test]
    fn the_allowlist_is_comma_separated_and_trimmed() {
        assert_eq!(
            parse_allowed_hosts("slack-mcp, slack-mcp:8000 ,localhost"),
            ["slack-mcp", "slack-mcp:8000", "localhost"]
        );
    }

    #[test]
    fn an_unset_or_blank_allowlist_falls_back_to_loopback_not_to_nothing() {
        // rmcp reads an empty list as "allow every Host". A compose file that forgets
        // the variable must fail closed and be obvious, not fail open and be invisible.
        for raw in ["", "   ", ",", " , , "] {
            assert_eq!(
                parse_allowed_hosts(raw),
                DEFAULT_ALLOWED_HOSTS,
                "{raw:?} must not disable the guard"
            );
        }
    }

    #[test]
    fn a_single_host_replaces_the_default_rather_than_extending_it() {
        // Production sets exactly `slack-mcp`; loopback is not silently kept alongside
        // it, so what the log line prints is the whole truth.
        assert_eq!(parse_allowed_hosts("slack-mcp"), ["slack-mcp"]);
    }
}
