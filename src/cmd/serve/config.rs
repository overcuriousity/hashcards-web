use std::env::current_dir;
use std::fs::read_to_string;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::cmd::drill::render::AnswerControls;
use crate::error::Fallible;
use crate::error::fail;
use crate::types::collection_id::CollectionId;
use crate::types::performance::DesiredRetention;
use crate::types::performance::Jitter;
use crate::types::performance::MaxInterval;
use crate::types::performance::Scheduling;

// --- TOML deserialization structs ---

#[derive(Deserialize)]
pub struct ServeConfig {
    pub server: ServerSection,
    #[serde(default)]
    pub defaults: DefaultsSection,
    #[serde(default)]
    pub oidc: Option<OidcSection>,
    #[serde(rename = "deck", default)]
    pub decks: Vec<CustomDeckEntry>,
}

/// A user-assembled deck: a named selection of decks drawn from any of the
/// owner's collections, drilled as one session.
///
/// `members` are `"{collection-slug}/{deck-name}"` pairs. Reviews still go to
/// each card's own collection database, so a card keeps exactly one schedule
/// no matter how many custom decks include it.
#[derive(Deserialize, Serialize, Clone)]
pub struct CustomDeckEntry {
    pub name: String,
    pub members: Vec<String>,
    #[serde(default)]
    pub owner: Option<String>,
}

/// One `members` entry, split into the collection it names and the deck
/// within it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeckMember {
    pub collection_slug: String,
    pub deck_name: String,
}

impl DeckMember {
    /// Parse `"{slug}/{deck}"`. Deck names may contain `/` (they mirror the
    /// file tree), so only the first separator splits.
    pub fn parse(raw: &str) -> Option<Self> {
        let (slug, deck) = raw.split_once('/')?;
        if slug.is_empty() || deck.is_empty() {
            return None;
        }
        Some(Self {
            collection_slug: slug.to_string(),
            deck_name: deck.to_string(),
        })
    }

    pub fn encode(&self) -> String {
        format!("{}/{}", self.collection_slug, self.deck_name)
    }
}

#[derive(Deserialize)]
pub struct ServerSection {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub data_dir: String,
    /// Evict drill sessions idle for this many minutes, closing their DB
    /// session row. 0 disables eviction. Default: 1440 (24 hours).
    #[serde(default = "default_session_timeout_minutes")]
    pub session_timeout_minutes: u64,
}

/// Default bind address. Deliberately localhost-only: without an `[oidc]`
/// section there is no authentication at all, so binding to every interface
/// must be an explicit opt-in (`host = "0.0.0.0"` in the config file).
fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8000
}

fn default_session_timeout_minutes() -> u64 {
    1440
}

#[derive(Deserialize)]
pub struct OidcSection {
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub external_url: String,
    pub session_secret: String,
    #[serde(default = "default_oidc_scopes")]
    pub scopes: Vec<String>,
}

fn default_oidc_scopes() -> Vec<String> {
    vec![
        "openid".to_string(),
        "email".to_string(),
        "profile".to_string(),
    ]
}

/// `Key::derive_from` (the cookie signing key) panics on anything shorter,
/// so this is the minimum a config may declare.
pub const MIN_SESSION_SECRET_BYTES: usize = 32;

#[derive(Clone)]
pub struct ResolvedOidc {
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub external_url: String,
    pub session_secret: String,
    pub scopes: Vec<String>,
}

#[derive(Deserialize)]
pub struct DefaultsSection {
    #[serde(default = "default_answer_controls")]
    pub answer_controls: AnswerControlsConfig,
    #[serde(default = "default_true")]
    pub bury_siblings: bool,
    #[serde(default = "default_jitter")]
    pub jitter: f64,
    #[serde(default = "default_desired_retention")]
    pub desired_retention: f64,
    #[serde(default = "default_max_interval_days")]
    pub max_interval_days: f64,
}

impl DefaultsSection {
    /// The instance-wide schedule shape, with every value checked. A
    /// collection may override the two FSRS numbers; jitter is per instance,
    /// since it exists to spread one person's review peaks and means nothing
    /// to one collection alone.
    pub fn scheduling(&self) -> Fallible<Scheduling> {
        Ok(Scheduling {
            retention: DesiredRetention::new(self.desired_retention)?,
            max_interval: MaxInterval::new(self.max_interval_days)?,
            jitter: Jitter::new(self.jitter)?,
        })
    }
}

fn default_jitter() -> f64 {
    Jitter::DEFAULT_FRACTION
}

fn default_desired_retention() -> f64 {
    DesiredRetention::DEFAULT
}

fn default_max_interval_days() -> f64 {
    MaxInterval::DEFAULT
}

impl Default for DefaultsSection {
    fn default() -> Self {
        Self {
            answer_controls: default_answer_controls(),
            bury_siblings: true,
            jitter: default_jitter(),
            desired_retention: default_desired_retention(),
            max_interval_days: default_max_interval_days(),
        }
    }
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum AnswerControlsConfig {
    Full,
    Binary,
}

fn default_answer_controls() -> AnswerControlsConfig {
    AnswerControlsConfig::Full
}

fn default_true() -> bool {
    true
}

impl From<AnswerControlsConfig> for AnswerControls {
    fn from(config: AnswerControlsConfig) -> Self {
        match config {
            AnswerControlsConfig::Full => AnswerControls::Full,
            AnswerControlsConfig::Binary => AnswerControls::Binary,
        }
    }
}

pub fn slugify(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

pub fn load_config(path: &Path) -> Fallible<ServeConfig> {
    let content = read_to_string(path)?;
    let config: ServeConfig = toml::from_str(&content)?;
    config.defaults.scheduling()?;
    Ok(config)
}

// --- Resolved runtime config ---

/// What a collection's own `.hashcards.toml` says about how its cards are
/// scheduled. Absent means "whatever the instance says".
#[derive(Clone, Copy, Default)]
pub struct SchedulingOverrides {
    pub retention: Option<DesiredRetention>,
    pub max_interval: Option<MaxInterval>,
}

#[derive(Clone)]
pub struct ResolvedCollection {
    pub name: String,
    pub slug: String,
    pub coll_dir: PathBuf,
    /// The owning user's review database, shared by every collection in
    /// their card tree. Which rows in it belong to this collection is
    /// `collection_id`, not the file name.
    pub db_path: PathBuf,
    pub collection_id: CollectionId,
    /// Owning user's email (lowercased), when `[oidc]` is configured.
    pub owner: Option<String>,
    /// Scheduling this collection asks for in place of the instance's.
    pub overrides: SchedulingOverrides,
}

impl ResolvedCollection {
    /// How this collection's cards are scheduled: its own settings where it
    /// has them, the instance's everywhere else.
    ///
    /// Jitter is never overridden. It spreads one person's review peaks
    /// across all their collections, so a collection deciding it alone would
    /// be deciding nothing.
    pub fn scheduling(&self, defaults: Scheduling) -> Scheduling {
        Scheduling {
            retention: self.overrides.retention.unwrap_or(defaults.retention),
            max_interval: self.overrides.max_interval.unwrap_or(defaults.max_interval),
            jitter: defaults.jitter,
        }
    }
}

pub struct ResolvedServeConfig {
    pub host: String,
    pub port: u16,
    pub defaults: DefaultsSection,
    /// The directory holding the card trees and the review databases.
    pub data_dir: Option<PathBuf>,
    /// Config file path; needed to persist UI changes back to disk.
    pub config_path: Option<PathBuf>,
    /// User-assembled cross-collection decks loaded from the config file.
    pub custom_decks: Vec<CustomDeckEntry>,
    /// Idle drill sessions are evicted after this many minutes (0 = never).
    pub session_timeout_minutes: u64,
    /// Set when `[oidc]` is configured. Gates every route except `/auth/*`
    /// behind login and scopes collections/notes to their `owner`.
    pub oidc: Option<ResolvedOidc>,
}

impl ResolvedServeConfig {
    pub fn from_toml(config: ServeConfig) -> Fallible<Self> {
        let data_dir = {
            let p = PathBuf::from(&config.server.data_dir);
            if p.is_absolute() {
                p
            } else {
                current_dir()?.join(p)
            }
        };
        let oidc = match config.oidc {
            None => None,
            Some(o) => {
                // `openid` is what makes this OpenID Connect rather than plain
                // OAuth, so it is required. `email` is not: a provider that
                // sends no email claim is identified by its subject instead,
                // which every ID token carries (see `callback_handler`). Most
                // deployments still want `email`, because a subject is an
                // opaque string that nobody wants to paste into an `owner`
                // key, so it stays in the default scope list.
                if !o.scopes.iter().any(|s| s == "openid") {
                    return fail("configuration error: [oidc].scopes must include `openid`");
                }
                // `Key::derive_from` requires at least 32 bytes and panics
                // below that, so a short secret has to be rejected here with
                // a message the operator can act on.
                if o.session_secret.len() < MIN_SESSION_SECRET_BYTES {
                    return fail(format!(
                        "configuration error: [oidc].session_secret must be at least {} bytes \
                         long (it is {}); generate one with `openssl rand -hex 32`",
                        MIN_SESSION_SECRET_BYTES,
                        o.session_secret.len()
                    ));
                }
                Some(ResolvedOidc {
                    issuer_url: o.issuer_url,
                    client_id: o.client_id,
                    client_secret: o.client_secret,
                    external_url: o.external_url,
                    session_secret: o.session_secret,
                    scopes: o.scopes,
                })
            }
        };

        let custom_decks: Vec<CustomDeckEntry> = config
            .decks
            .iter()
            .map(|d| CustomDeckEntry {
                name: d.name.clone(),
                members: d.members.clone(),
                owner: d.owner.as_ref().map(|o| o.to_lowercase()),
            })
            .collect();
        for deck in &custom_decks {
            if deck.name.trim().is_empty() {
                return fail("configuration error: a [[deck]] entry has an empty `name`");
            }
            for raw in &deck.members {
                if DeckMember::parse(raw).is_none() {
                    return fail(format!(
                        "configuration error: [[deck]] '{}' has the member `{raw}`, which is not \
                         in `collection-slug/deck-name` form",
                        deck.name
                    ));
                }
            }
        }

        if oidc.is_some() {
            for d in &custom_decks {
                if d.owner.is_none() {
                    return fail(format!(
                        "configuration error: [oidc] is enabled, so every [[deck]] entry must \
                         declare an `owner`, but the deck '{}' has none",
                        d.name
                    ));
                }
            }
        } else {
            // Without `[oidc]` every request is unauthenticated, so `owner`
            // can never match and the entry would simply be unreachable.
            // Silently ignoring the field would hide that; refuse instead.
            if let Some(d) = custom_decks.iter().find(|d| d.owner.is_some()) {
                return fail(format!(
                    "configuration error: the [[deck]] entry '{}' declares an `owner`, but \
                     [oidc] is not configured, so nobody is ever logged in and the deck would \
                     be unreachable. Add an [oidc] section or remove the `owner`",
                    d.name
                ));
            }
        }

        Ok(Self {
            host: config.server.host,
            port: config.server.port,
            defaults: config.defaults,
            data_dir: Some(data_dir),
            config_path: None,
            custom_decks,
            session_timeout_minutes: config.server.session_timeout_minutes,
            oidc,
        })
    }

    pub fn with_config_path(mut self, path: PathBuf) -> Self {
        self.config_path = Some(path);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorReport;
    use crate::error::Fallible;

    /// The two FSRS knobs default to what the hardcoded constants were, so a
    /// config that says nothing about scheduling schedules as it always did.
    #[test]
    fn test_scheduling_defaults_to_the_previous_constants() -> Fallible<()> {
        let toml = "[server]\ndata_dir = \"/var/lib/hashcards\"\n";
        let config: ServeConfig = toml::from_str(toml)?;
        let scheduling = config.defaults.scheduling()?;
        assert_eq!(scheduling.retention.into_inner(), DesiredRetention::DEFAULT);
        assert_eq!(scheduling.max_interval.into_inner(), MaxInterval::DEFAULT);
        Ok(())
    }

    #[test]
    fn test_scheduling_is_read_from_defaults() -> Fallible<()> {
        let toml = "[server]\ndata_dir = \"/var/lib/hashcards\"\n\n\
                    [defaults]\ndesired_retention = 0.95\nmax_interval_days = 3650\n";
        let config: ServeConfig = toml::from_str(toml)?;
        let scheduling = config.defaults.scheduling()?;
        assert_eq!(scheduling.retention.into_inner(), 0.95);
        assert_eq!(scheduling.max_interval.into_inner(), 3650.0);
        Ok(())
    }

    /// A scheduling value out of range must stop the server at startup. It
    /// cannot wait to be noticed: the first symptom otherwise is a due date
    /// that is wrong, weeks later, with nothing to attribute it to.
    #[test]
    fn test_out_of_range_scheduling_is_refused_at_load() {
        let bad = |line: &str| {
            format!("[server]\ndata_dir = \"/var/lib/hashcards\"\n\n[defaults]\n{line}\n")
        };
        for line in [
            "desired_retention = 1.0",
            "desired_retention = 0.5",
            "max_interval_days = 0",
            "max_interval_days = 40000",
        ] {
            let config: ServeConfig = toml::from_str(&bad(line)).expect("the toml itself parses");
            assert!(
                config.defaults.scheduling().is_err(),
                "`{line}` was accepted"
            );
        }
    }

    /// Regression test for BUG-47: with no `host` key in the config, the
    /// server must bind to localhost, not to all interfaces.
    #[test]
    fn test_default_host_is_localhost() -> Fallible<()> {
        let toml = "[server]\ndata_dir = \"/var/lib/hashcards\"\n";
        let config: ServeConfig = toml::from_str(toml)?;
        assert_eq!(config.server.host, "127.0.0.1");
        Ok(())
    }

    /// `email` is no longer a required scope: a provider that sends no email
    /// claim identifies the user by subject instead. Only `openid` is
    /// mandatory, since without it this is not OpenID Connect at all.
    #[test]
    fn test_oidc_scopes_need_only_openid() -> Fallible<()> {
        let data_dir = current_dir()?.join("var-lib-hashcards-oidc-scopes-test");
        let config_toml = |scopes: &str| {
            format!(
                "[server]\ndata_dir = {:?}\n\n\
                 [oidc]\n\
                 issuer_url = \"https://idp.example.com\"\n\
                 client_id = \"abc\"\n\
                 client_secret = \"secret\"\n\
                 external_url = \"https://hashcards.example.com\"\n\
                 session_secret = \"a-very-long-random-session-secret-value\"\n\
                 scopes = {scopes}\n",
                data_dir
            )
        };

        let config: ServeConfig = toml::from_str(&config_toml("[\"openid\"]"))?;
        ResolvedServeConfig::from_toml(config)
            .map_err(|e| ErrorReport::new(format!("`openid` alone must be accepted: {e}")))?;

        let config: ServeConfig = toml::from_str(&config_toml("[\"email\", \"profile\"]"))?;
        match ResolvedServeConfig::from_toml(config) {
            Ok(_) => panic!("expected an error for scopes without `openid`"),
            Err(e) => assert!(
                e.to_string().contains("openid"),
                "the error must name the missing scope: {e}"
            ),
        }
        Ok(())
    }

    /// `Key::derive_from` panics below 32 bytes, so a short `session_secret`
    /// must be rejected at config load with a message the operator can act
    /// on rather than crashing the server at startup.
    #[test]
    fn test_short_session_secret_is_rejected() -> Fallible<()> {
        let data_dir = current_dir()?.join("var-lib-hashcards-short-secret-test");
        let toml_str = format!(
            "[server]\ndata_dir = {:?}\n\n\
             [oidc]\n\
             issuer_url = \"https://idp.example.com\"\n\
             client_id = \"abc\"\n\
             client_secret = \"secret\"\n\
             external_url = \"https://hashcards.example.com\"\n\
             session_secret = \"hunter2\"\n\n\
             [[collection]]\n\
             name = \"Japanese\"\n\
             path = \"japanese\"\n\
             owner = \"me@example.com\"\n",
            data_dir
        );
        let config: ServeConfig = toml::from_str(&toml_str)?;
        match ResolvedServeConfig::from_toml(config) {
            Ok(_) => panic!("expected an error for a session_secret shorter than 32 bytes"),
            Err(e) => assert!(
                e.to_string().contains("session_secret"),
                "error should name the offending setting: {e}"
            ),
        }
        Ok(())
    }

    /// Windows CI regression: `data_dir` is interpolated into TOML with
    /// `{:?}`, not `{}`, so a path containing backslashes stays a valid TOML
    /// basic string. Formatting it raw produced `data_dir = "D:\a\..."`,
    /// which the parser rejects as a bad escape. This test runs everywhere so
    /// the fix cannot regress on a Linux-only run.
    #[test]
    fn test_windows_style_data_dir_survives_toml_formatting() -> Fallible<()> {
        let data_dir = PathBuf::from(r"D:\a\hashcards-web\var-lib-hashcards");
        let toml_str = format!(
            "[server]\ndata_dir = {:?}\n\n\
             [[collection]]\n\
             name = \"Japanese\"\n\
             path = \"japanese\"\n",
            data_dir
        );
        let config: ServeConfig = toml::from_str(&toml_str)?;
        assert_eq!(PathBuf::from(&config.server.data_dir), data_dir);
        Ok(())
    }

    /// A `[[deck]]` member must be `collection-slug/deck-name`.
    #[test]
    fn test_malformed_deck_member_is_rejected() -> Fallible<()> {
        let data_dir = current_dir()?.join("var-lib-hashcards-deck-member-test");
        let toml_str = format!(
            "[server]\ndata_dir = {:?}\n\n\
             [[deck]]\n\
             name = \"Mixed\"\n\
             members = [\"no-separator\"]\n",
            data_dir
        );
        let config: ServeConfig = toml::from_str(&toml_str)?;
        match ResolvedServeConfig::from_toml(config) {
            Ok(_) => panic!("expected an error for a malformed [[deck]] member"),
            Err(e) => assert!(
                e.to_string().contains("Mixed") && e.to_string().contains("no-separator"),
                "error should name the deck and the bad member: {e}"
            ),
        }
        Ok(())
    }

    /// With `[oidc]` on, every deck needs an owner, exactly like collections
    /// do.
    #[test]
    fn test_oidc_requires_owner_on_every_deck() -> Fallible<()> {
        let data_dir = current_dir()?.join("var-lib-hashcards-deck-owner-test");
        let toml_str = format!(
            "[server]\ndata_dir = {:?}\n\n\
             [oidc]\n\
             issuer_url = \"https://idp.example.com\"\n\
             client_id = \"abc\"\n\
             client_secret = \"secret\"\n\
             external_url = \"https://hashcards.example.com\"\n\
             session_secret = \"a-very-long-random-session-secret-value\"\n\n\
             [[deck]]\n\
             name = \"Mixed\"\n\
             members = [\"japanese/Verbs\"]\n",
            data_dir
        );
        let config: ServeConfig = toml::from_str(&toml_str)?;
        match ResolvedServeConfig::from_toml(config) {
            Ok(_) => panic!("expected an error for a deck with no owner while [oidc] is set"),
            Err(e) => assert!(
                e.to_string().contains("Mixed") && e.to_string().contains("owner"),
                "error should name the deck and mention `owner`: {e}"
            ),
        }
        Ok(())
    }

    /// A deck owner is lowercased for case-insensitive matching, and a deck
    /// with no `[oidc]` may not declare one at all.
    #[test]
    fn test_deck_owner_is_lowercased_and_rejected_without_oidc() -> Fallible<()> {
        let data_dir = current_dir()?.join("var-lib-hashcards-deck-owner-case-test");
        let with_oidc = format!(
            "[server]\ndata_dir = {:?}\n\n\
             [oidc]\n\
             issuer_url = \"https://idp.example.com\"\n\
             client_id = \"abc\"\n\
             client_secret = \"secret\"\n\
             external_url = \"https://hashcards.example.com\"\n\
             session_secret = \"a-very-long-random-session-secret-value\"\n\n\
             [[deck]]\n\
             name = \"Mixed\"\n\
             members = [\"japanese/Verbs\"]\n\
             owner = \"Me@Example.com\"\n",
            data_dir
        );
        let config: ServeConfig = toml::from_str(&with_oidc)?;
        let resolved = ResolvedServeConfig::from_toml(config)?;
        assert_eq!(
            resolved.custom_decks[0].owner.as_deref(),
            Some("me@example.com")
        );

        let without_oidc = format!(
            "[server]\ndata_dir = {:?}\n\n\
             [[deck]]\n\
             name = \"Mixed\"\n\
             members = [\"japanese/Verbs\"]\n\
             owner = \"me@example.com\"\n",
            data_dir
        );
        let config: ServeConfig = toml::from_str(&without_oidc)?;
        assert!(
            ResolvedServeConfig::from_toml(config).is_err(),
            "an `owner` without [oidc] must be rejected for decks too"
        );
        Ok(())
    }
}
