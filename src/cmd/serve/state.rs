use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use axum_extra::extract::cookie::Key;
use chrono::Duration;
use parking_lot::Mutex;

use crate::auth_db::AuthDatabase;
use crate::cmd::drill::render::AnswerControls;
use crate::cmd::drill::state::CardMigration;
use crate::cmd::drill::state::MigrationEffect;
use crate::cmd::drill::state::MutableState;
use crate::cmd::serve::auth::OidcRuntime;
use crate::cmd::serve::config::ResolvedServeConfig;
use crate::cmd::serve::decks::ResolvedCustomDeck;
use crate::types::collection_id::CollectionId;
use crate::types::timestamp::Timestamp;

/// A drill session shared behind a per-session lock. Handlers clone the
/// `Arc` out of the map (releasing the map lock immediately) and lock the
/// session itself for the duration of the request, so an error can never
/// remove the session from the map.
pub type SharedSession = Arc<Mutex<DrillSession>>;

/// What a running drill session is filed under: the slug it is addressed by,
/// and who it belongs to.
///
/// The owner is part of the key, not decoration. A collection is a folder in
/// its owner's card tree, so two users may each own a "Spanish" and both
/// slugify to `Spanish`; keyed by slug alone, whoever asked second was
/// handed the other's live session and could grade into their database.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    /// The owner's email, or `None` when `[oidc]` is off and there is only
    /// the shared `default` tree.
    owner: Option<String>,
    slug: String,
}

impl SessionKey {
    pub fn new(owner: Option<&str>, slug: &str) -> Self {
        Self {
            owner: owner.map(|o| o.to_string()),
            slug: slug.to_string(),
        }
    }

    pub fn slug(&self) -> &str {
        &self.slug
    }

    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ResolvedServeConfig>,
    pub sessions: Arc<Mutex<HashMap<SessionKey, SharedSession>>>,
    /// User-assembled cross-collection decks, resolved at startup and
    /// refreshed whenever one is added or deleted. A `parking_lot` mutex
    /// rather than an async one: these are read from the blocking drill
    /// paths, not only from async handlers.
    pub custom_decks: Arc<Mutex<Vec<ResolvedCustomDeck>>>,
    pub config_path: Arc<Mutex<Option<PathBuf>>>,
    /// Per-database count of session rows closed by the startup sweep,
    /// waiting to be reported to the user. The topic browser takes the entry
    /// the first time it renders, so the notice is shown once rather than on
    /// every visit (see `sweep_dangling_sessions`).
    ///
    /// Keyed by collection id — not by slug, and no longer by database path:
    /// two users may each own a collection called "Spanish", and after
    /// consolidation every collection in one tree shares a database file.
    pub interrupted_closed: Arc<Mutex<HashMap<CollectionId, usize>>>,
    /// Users whose startup merge failed, keyed by their review database's
    /// path and holding the error to show them.
    ///
    /// Their collections refuse to open rather than starting from an empty
    /// database. A merge that half-succeeded is a transaction that rolled
    /// back, so no rows were lost — but the sources are still in place and
    /// the target is not what it should be, and serving that as though it
    /// were a fresh account is how a person concludes their history is gone.
    pub migration_failures: Arc<HashMap<PathBuf, String>>,
    /// Signs the OIDC session and login-flow cookies. When `[oidc]` is not
    /// configured this key is generated randomly at startup and never used
    /// — keeping it non-optional avoids threading `Option` through every
    /// cookie read/write, since the auth routes and middleware that use it
    /// are only ever registered when `[oidc]` is configured.
    pub session_key: Key,
    /// Set when `[oidc]` is configured. Gates every route except `/auth/*`
    /// behind login and scopes collections/notes to their `owner`.
    pub oidc: Option<Arc<OidcRuntime>>,
    /// The server's MCP token store, at `{data_dir}/auth.db`.
    ///
    /// `None` only when no data directory is configured, which is the same
    /// condition under which there is nothing to serve.
    pub auth: Option<Arc<AuthDatabase>>,
}

/// Lets `axum_extra`'s `SignedCookieJar` extractor pull the signing key
/// straight out of `AppState`.
impl axum::extract::FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Key {
        state.session_key.clone()
    }
}

pub struct CollectionInfo {
    pub name: String,
    pub slug: String,
    pub total_cards: usize,
    pub due_today: usize,
    pub owner: Option<String>,
}

pub struct DrillSession {
    pub directory: PathBuf,
    pub macros: Vec<(String, String)>,
    pub total_cards: usize,
    pub session_started_at: Timestamp,
    /// Last time this session served a page or handled an action.
    /// Sessions idle past the configured timeout are evicted (BUG-08).
    pub last_activity_at: Timestamp,
    pub answer_controls: AnswerControls,
    pub mutable: MutableState,
    /// True once this session has been taken out of the sessions map (ended
    /// via Home, evicted as idle, or replaced by a new drill).
    ///
    /// Request handlers need to know that the session they cloned out of the
    /// map is still the one the map holds. Re-reading the map would mean
    /// taking the map lock while holding the session lock, inverting the
    /// global lock order (map, then session) that eviction and the landing
    /// page follow -- a deadlock with no timeout. Whoever removes a session
    /// from the map instead marks it here, under the session lock it already
    /// has to take, and handlers just read the flag.
    detached: bool,
}

impl DrillSession {
    pub fn new(
        directory: PathBuf,
        macros: Vec<(String, String)>,
        session_started_at: Timestamp,
        answer_controls: AnswerControls,
        mutable: MutableState,
    ) -> Self {
        Self {
            directory,
            macros,
            total_cards: mutable.cards.len(),
            session_started_at,
            last_activity_at: session_started_at,
            answer_controls,
            mutable,
            detached: false,
        }
    }

    /// Mark this session as removed from the sessions map.
    ///
    /// Callers must already hold the map lock, so that the mark and the
    /// removal are seen together.
    pub fn detach(&mut self) {
        self.detached = true;
    }

    /// Has this session been removed from the sessions map?
    pub fn is_detached(&self) -> bool {
        self.detached
    }
}

/// Remove sessions idle for at least `timeout_minutes` and close their DB
/// session rows. `timeout_minutes == 0` disables eviction. Returns the
/// evicted keys.
///
/// DB work happens after the map lock is released, so eviction never holds
/// the sessions lock across SQLite calls.
pub fn evict_idle_sessions(
    sessions: &Mutex<HashMap<SessionKey, SharedSession>>,
    timeout_minutes: u64,
    now: Timestamp,
) -> Vec<SessionKey> {
    if timeout_minutes == 0 {
        return Vec::new();
    }
    let timeout = Duration::minutes(timeout_minutes as i64);
    let expired: Vec<(SessionKey, SharedSession)> = {
        let mut map = sessions.lock();
        let keys: Vec<SessionKey> = map
            .iter()
            .filter(|(_, s)| now.into_inner() - s.lock().last_activity_at.into_inner() >= timeout)
            .map(|(k, _)| k.clone())
            .collect();
        keys.into_iter()
            .filter_map(|k| map.remove(&k).map(|s| (k, s)))
            .inspect(|(_, s)| s.lock().detach())
            .collect()
    };
    let mut evicted = Vec::new();
    for (key, session) in expired {
        let session = session.lock();
        if session.mutable.finished_at.is_none() {
            if let Err(e) = session.mutable.dbs.close_all(now) {
                log::error!(
                    "Failed to close evicted session for collection '{}': {e}",
                    key.slug()
                );
            }
        }
        evicted.push(key);
    }
    evicted
}

/// Every live session drawing cards from `coll_dir`.
///
/// Answered from the sessions themselves rather than from deck
/// configuration: a session holds what it actually loaded when it started,
/// which is the question being asked, and the configuration may have moved
/// since. It also closes a bug at its root — a custom deck has its own key
/// in the sessions map, so looking the *collection's* slug up in the map
/// misses a running deck session that includes it.
///
/// A session drawing on several collections records each one in its
/// `SessionDb.source`. A single-collection session leaves every `source` as
/// `None` and is identified by its own directory instead.
///
/// Follows the map-then-session lock order: the map lock is released before
/// any session lock is taken.
pub fn sessions_touching(state: &AppState, coll_dir: &Path) -> Vec<SharedSession> {
    let candidates: Vec<SharedSession> = state.sessions.lock().values().cloned().collect();
    candidates
        .into_iter()
        .filter(|shared| {
            let session = shared.lock();
            if session.is_detached() {
                return false;
            }
            let mut routed = false;
            for entry in session.mutable.dbs.all() {
                if let Some(source) = &entry.source {
                    routed = true;
                    if same_dir(&source.coll_dir, coll_dir) {
                        return true;
                    }
                }
            }
            !routed && same_dir(&session.directory, coll_dir)
        })
        .collect()
}

/// Apply `migration` to every live session drawing on `coll_dir`, and
/// report the total effect.
///
/// Called only after the file has been written and the database
/// transaction has committed, which is what lets it be infallible.
///
/// A session that started between the write and this call looks like a
/// race and is not: a session parses its collection when it starts, so one
/// beginning after the write already holds the new hashes, and the
/// migration — which looks up old ones — finds nothing and does nothing. A
/// session that began before the write is in the map.
pub fn migrate_sessions(
    state: &AppState,
    coll_dir: &Path,
    migration: &CardMigration,
) -> MigrationEffect {
    let mut total = MigrationEffect {
        renamed: 0,
        dropped: 0,
        session_finished: false,
    };
    for shared in sessions_touching(state, coll_dir) {
        let mut session = shared.lock();
        let effect = session.mutable.apply_card_migration(migration);
        // The progress bar's denominator counts the cards the session set
        // out with; an edit that removed some must not leave it stuck.
        session.total_cards = session.total_cards.saturating_sub(effect.dropped);
        total.renamed += effect.renamed;
        total.dropped += effect.dropped;
        total.session_finished |= effect.session_finished;
    }
    total
}

/// Whether two paths name the same directory.
///
/// Canonicalized first: one caller has the path the collection was
/// discovered at and another has it after `canonicalize`, and a symlinked
/// or `..`-bearing spelling of the same directory must not read as a
/// different collection. A path that cannot be canonicalized — it was just
/// deleted, say — falls back to a literal comparison.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Test-only constructor for `AppState`.
#[cfg(test)]
pub mod test_support {
    use super::*;
    use crate::cmd::serve::config::DefaultsSection;
    use crate::cmd::serve::config::ResolvedServeConfig;

    /// An `AppState` whose card trees live under `data_dir`, with no OIDC
    /// runtime and no saved decks.
    pub fn state_with_data_dir(data_dir: PathBuf) -> AppState {
        AppState {
            config: Arc::new(ResolvedServeConfig {
                host: "127.0.0.1".to_string(),
                port: 0,
                defaults: DefaultsSection::default(),
                data_dir: Some(data_dir),
                config_path: None,
                custom_decks: Vec::new(),
                session_timeout_minutes: 1440,
                oidc: None,
            }),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            custom_decks: Arc::new(Mutex::new(Vec::new())),
            config_path: Arc::new(Mutex::new(None)),
            interrupted_closed: Arc::new(Mutex::new(HashMap::new())),
            migration_failures: Arc::new(HashMap::new()),
            session_key: Key::generate(),
            oidc: None,
            // Tests that need one open it themselves: most do not touch
            // tokens at all, and opening a database per state would make
            // every unrelated test pay for it.
            auth: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::tempdir;

    use super::*;
    use crate::cmd::drill::cache::Cache;
    use crate::cmd::drill::state::SessionDbs;
    use crate::error::Fallible;
    use crate::rng::TinyRng;
    use crate::types::performance::Jitter;
    use crate::types::performance::Scheduling;

    /// A view on the user database at `path`, under a fixed collection id:
    /// these tests are about session bookkeeping, not about scoping.
    fn test_db(path: &Path) -> Fallible<crate::db::Database> {
        Ok(crate::user_db::UserDatabase::open(path)?.collection(
            crate::types::collection_id::CollectionId::new("test-collection")?,
        ))
    }

    fn session_started_at(at: &str, dir: &Path) -> Fallible<SharedSession> {
        let db = test_db(&dir.join("test.db"))?;
        let started_at = Timestamp::try_from(at.to_string())?;
        let session_id = db.create_session(started_at)?;
        let mutable = MutableState::new(
            SessionDbs::single(
                db,
                session_id,
                Scheduling {
                    jitter: Jitter::none(),
                    ..Scheduling::default()
                },
            ),
            Cache::new(),
            Vec::new(),
            TinyRng::from_seed(1),
        );
        Ok(Arc::new(Mutex::new(DrillSession::new(
            dir.to_path_buf(),
            Vec::new(),
            started_at,
            AnswerControls::Full,
            mutable,
        ))))
    }

    /// BUG-08 regression: a session idle past the timeout is removed from
    /// the map and its DB session row is closed.
    #[test]
    fn test_idle_session_is_evicted_and_db_row_closed() -> Fallible<()> {
        let dir = tempdir()?;
        let session = session_started_at("2026-01-01T10:00:00.000", dir.path())?;
        let sessions = Mutex::new(HashMap::from([(SessionKey::new(None, "demo"), session)]));

        // 25 hours later, with a 24h (1440 minute) timeout.
        let now = Timestamp::try_from("2026-01-02T11:00:00.000".to_string())?;
        let evicted = evict_idle_sessions(&sessions, 1440, now);
        assert_eq!(evicted, vec![SessionKey::new(None, "demo")]);
        assert!(sessions.lock().is_empty());

        // The DB session row was closed with the eviction time.
        let db = test_db(&dir.path().join("test.db"))?;
        let rows = db.get_all_sessions()?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ended_at, now);
        Ok(())
    }

    /// Eviction must mark the session detached, not merely drop it from the
    /// map: a request handler that already cloned the `Arc` learns the
    /// session is gone by reading this flag. Re-reading the map under the
    /// session lock would invert the global lock order and deadlock.
    #[test]
    fn test_evicted_session_is_marked_detached() -> Fallible<()> {
        let dir = tempdir()?;
        let session = session_started_at("2026-01-01T10:00:00.000", dir.path())?;
        // A handler holding its own clone, as one would across a request.
        let handler_view = Arc::clone(&session);
        assert!(!handler_view.lock().is_detached());

        let sessions = Mutex::new(HashMap::from([(SessionKey::new(None, "demo"), session)]));
        let now = Timestamp::try_from("2026-01-02T11:00:00.000".to_string())?;
        assert_eq!(
            evict_idle_sessions(&sessions, 1440, now),
            vec![SessionKey::new(None, "demo")]
        );

        assert!(
            handler_view.lock().is_detached(),
            "evicted session must be observable as detached through an \
             already-cloned handle"
        );
        Ok(())
    }

    /// A session inside the timeout window is left alone.
    #[test]
    fn test_active_session_is_not_evicted() -> Fallible<()> {
        let dir = tempdir()?;
        let session = session_started_at("2026-01-01T10:00:00.000", dir.path())?;
        let sessions = Mutex::new(HashMap::from([(SessionKey::new(None, "demo"), session)]));

        // Only one hour later.
        let now = Timestamp::try_from("2026-01-01T11:00:00.000".to_string())?;
        let evicted = evict_idle_sessions(&sessions, 1440, now);
        assert!(evicted.is_empty());
        assert_eq!(sessions.lock().len(), 1);
        Ok(())
    }

    /// A timeout of 0 disables eviction entirely.
    #[test]
    fn test_zero_timeout_disables_eviction() -> Fallible<()> {
        let dir = tempdir()?;
        let session = session_started_at("2020-01-01T10:00:00.000", dir.path())?;
        let sessions = Mutex::new(HashMap::from([(SessionKey::new(None, "demo"), session)]));
        let now = Timestamp::try_from("2026-01-01T10:00:00.000".to_string())?;
        let evicted = evict_idle_sessions(&sessions, 0, now);
        assert!(evicted.is_empty());
        assert_eq!(sessions.lock().len(), 1);
        Ok(())
    }
}
