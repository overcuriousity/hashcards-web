use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use axum::routing::post;
use axum_extra::extract::cookie::Key;
use tokio::net::TcpListener;

use crate::cmd::drill::fonts::font_handler;
use crate::cmd::drill::fonts::legacy_font_handler;
use crate::cmd::drill::hljs::hljs_css_handler;
use crate::cmd::drill::hljs::hljs_js_handler;
use crate::cmd::drill::hljs::legacy_hljs_css_handler;
use crate::cmd::drill::hljs::legacy_hljs_js_handler;
use crate::cmd::drill::katex::katex_css_handler;
use crate::cmd::drill::katex::katex_font_handler;
use crate::cmd::drill::katex::katex_js_handler;
use crate::cmd::drill::katex::katex_mhchem_js_handler;
use crate::cmd::drill::katex::legacy_katex_css_handler;
use crate::cmd::drill::katex::legacy_katex_font_handler;
use crate::cmd::drill::katex::legacy_katex_js_handler;
use crate::cmd::drill::katex::legacy_katex_mhchem_js_handler;
use crate::cmd::drill::template::STYLE_CSS;
use crate::cmd::drill::template::STYLE_REV;
use crate::cmd::drill::template::icon_192_handler;
use crate::cmd::drill::template::icon_512_handler;
use crate::cmd::drill::template::manifest_handler;
use crate::cmd::serve::auth::build_oidc_runtime;
use crate::cmd::serve::auth::callback_handler;
use crate::cmd::serve::auth::login_handler;
use crate::cmd::serve::auth::logout_handler;
use crate::cmd::serve::auth::require_auth;
use crate::cmd::serve::bookmarks::bookmark_delete_handler;
use crate::cmd::serve::bookmarks::bookmark_list_handler;
use crate::cmd::serve::bookmarks::bookmark_note_handler;
use crate::cmd::serve::cards::discover_all_collections;
use crate::cmd::serve::config::MIN_SESSION_SECRET_BYTES;
use crate::cmd::serve::config::ResolvedOidc;
use crate::cmd::serve::config::ResolvedServeConfig;
use crate::cmd::serve::decks::check_deck_slug_collisions;
use crate::cmd::serve::decks::deck_add_handler;
use crate::cmd::serve::decks::deck_delete_handler;
use crate::cmd::serve::decks::decks_manage_handler;
use crate::cmd::serve::decks::resolve_custom_decks;
use crate::cmd::serve::edit::edit_get_handler;
use crate::cmd::serve::edit::edit_post_handler;
use crate::cmd::serve::export::collection_export_handler;
use crate::cmd::serve::files::editor_get_handler;
use crate::cmd::serve::files::editor_post_handler;
use crate::cmd::serve::files::files_delete_handler;
use crate::cmd::serve::files::files_file_handler;
use crate::cmd::serve::files::files_folder_handler;
use crate::cmd::serve::files::files_get_handler;
use crate::cmd::serve::files::files_rename_handler;
use crate::cmd::serve::files::preview_handler;
use crate::cmd::serve::handlers::collection_file_handler;
use crate::cmd::serve::handlers::collection_get_handler;
use crate::cmd::serve::handlers::collection_post_handler;
use crate::cmd::serve::handlers::collection_script_handler;
use crate::cmd::serve::handlers::collection_start_handler;
use crate::cmd::serve::landing::landing_handler;
use crate::cmd::serve::merge::merge_legacy_databases;
use crate::cmd::serve::state::AppState;
use crate::cmd::serve::state::SessionKey;
use crate::cmd::serve::state::SharedSession;
use crate::cmd::serve::state::evict_idle_sessions;
use crate::cmd::serve::stats::collection_stats_handler;
use crate::cmd::serve::upload::MAX_UPLOAD_BYTES;
use crate::cmd::serve::upload::media_upload_handler;
use crate::cmd::signals::terminate_signal;
use crate::error::Fallible;
use crate::error::fail;
use crate::types::timestamp::Timestamp;
use crate::user_db::UserDatabase;
use crate::utils::ensure_dir;

/// How long a session's heartbeat must have been silent before the startup
/// sweep treats it as abandoned. Generous on purpose: closing a session that
/// is merely idle in another process is worse than leaving a crashed one open
/// until the next restart. This does not eliminate the race, only shrinks its
/// window: a CLI `drill` session idle longer than this, in a database this
/// server also serves, is closed if `serve` happens to restart during that
/// window. Accepted tradeoff — any fixed cutoff has some such window.
const SESSION_STALE_MINUTES: i64 = 60;

/// Close session rows left dangling by a crash or restart, across every
/// collection this server can serve, and return the per-database counts so
/// the topic browser can report them once.
///
/// Keyed by database path rather than by URL slug: two users may each own a
/// collection called "Spanish", and a slug-keyed notice would be shown to
/// whichever of them opened the page first.
///
/// A collection whose database cannot be opened is skipped with a log line
/// rather than failing startup: an unreadable database is the topic
/// browser's problem to report, not a reason to refuse to serve everything
/// else. Each database is independent, so the sweeps run on their own
/// threads rather than one after another.
fn sweep_dangling_sessions(
    data_dir: &Path,
    failures: &HashMap<PathBuf, String>,
) -> HashMap<PathBuf, usize> {
    // Only sessions whose heartbeat has been silent this long are presumed
    // dead. A session in another process stamps its heartbeat as the user
    // works, so a live one is never swept out from under it.
    let stale_before = Timestamp::now().minus_minutes(SESSION_STALE_MINUTES);
    let collections = discover_all_collections(data_dir);

    let results: Vec<(PathBuf, Fallible<usize>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = collections
            .iter()
            .map(|rc| {
                scope.spawn(move || {
                    if failures.contains_key(&rc.db_path) {
                        // Already reported at startup, and the file is not
                        // what it should be. Do not write to it.
                        return (rc.db_path.clone(), Ok(0));
                    }
                    // Not `open_collection_db`: the sweep runs before
                    // `AppState` exists, so it consults the failure map
                    // directly, just above.
                    let closed = UserDatabase::open(&rc.db_path).and_then(|user| {
                        user.collection(rc.collection_id.clone())
                            .close_dangling_sessions(stale_before)
                    });
                    (rc.db_path.clone(), closed)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("sweep thread panicked"))
            .collect()
    });

    let mut counts = HashMap::new();
    for (db_path, closed) in results {
        match closed {
            Ok(0) => {}
            Ok(n) => {
                log::info!("Closed {n} interrupted session(s) in {}", db_path.display());
                counts.insert(db_path, n);
            }
            Err(e) => log::error!(
                "Could not close interrupted sessions in {}: {e}",
                db_path.display()
            ),
        }
    }
    counts
}

/// Derive the cookie signing key from `[oidc].session_secret`.
///
/// `Key::derive_from` panics on a secret shorter than
/// `MIN_SESSION_SECRET_BYTES`. `ResolvedServeConfig::from_toml` rejects those
/// already, but a config built any other way (tests, future callers) would
/// otherwise reach the panic, so the length is re-checked here and reported
/// as an ordinary error.
fn session_key(oidc: Option<&ResolvedOidc>) -> Fallible<Key> {
    match oidc {
        // Never used when `[oidc]` is absent: the routes and middleware that
        // read cookies are only registered when it is present.
        None => Ok(Key::generate()),
        Some(o) if o.session_secret.len() >= MIN_SESSION_SECRET_BYTES => {
            Ok(Key::derive_from(o.session_secret.as_bytes()))
        }
        Some(o) => fail(format!(
            "configuration error: [oidc].session_secret must be at least {} bytes long \
             (it is {}); generate one with `openssl rand -hex 32`",
            MIN_SESSION_SECRET_BYTES,
            o.session_secret.len()
        )),
    }
}

pub async fn start_serve(config: ResolvedServeConfig) -> Fallible<()> {
    // Everything the server writes lives under `data_dir`, so it is created
    // and proved writable here rather than at the first write.
    //
    // `data_dir` has no default and the shipped example says
    // `/var/lib/hashcards`, which on a systemd deployment only exists, and is
    // only owned by the service user, when the unit declares
    // `StateDirectory=hashcards`. Without that nothing created it, and the
    // first attempt to write -- creating a card folder, minutes or restarts
    // later -- failed with a bare "Permission denied (os error 13)" naming no
    // path at all.
    if let Some(data_dir) = &config.data_dir {
        ensure_dir(data_dir, "data directory")?;
        ensure_dir(&data_dir.join("db"), "review database directory")?;
    }

    let config_path: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(config.config_path.clone()));
    let bind = format!("{}:{}", config.host, config.port);

    let config = Arc::new(config);

    // The upgrade: fold any pre-consolidation per-collection databases into
    // one database per user. Before the sweep, which opens the databases
    // this produces. A tree whose merge failed is recorded rather than
    // fatal: one broken file must not take an instance down for everybody.
    let migration_failures = match &config.data_dir {
        Some(data_dir) => merge_legacy_databases(data_dir),
        None => HashMap::new(),
    };
    for (path, why) in &migration_failures {
        log::error!(
            "The review database at {} was not consolidated ({why}). The collections that use it \
             will refuse to open until this is fixed; their previous databases have not been \
             deleted.",
            path.display()
        );
    }

    // FEAT-03: close session rows left open by a crash or restart, once, at
    // startup. They cannot be resumed (the card queue lives only in memory),
    // so they are closed with all persisted reviews kept. This must not run
    // per request: the predicate cannot distinguish a crashed session from a
    // live one, and a second server may share the same database.
    let interrupted_closed = match &config.data_dir {
        Some(data_dir) => sweep_dangling_sessions(data_dir, &migration_failures),
        None => HashMap::new(),
    };

    // Discovery happens once at startup, not lazily on the first login
    // attempt, so a broken [oidc] config fails fast with a clear error.
    let oidc = match &config.oidc {
        Some(o) => Some(Arc::new(build_oidc_runtime(o).await?)),
        None => None,
    };

    // User-assembled decks are resolved once here; adds and deletes refresh
    // the list in place.
    let custom_decks = resolve_custom_decks(&config.custom_decks);
    // Nothing to check a deck against at startup any more: collections are
    // discovered per request, from each caller's own tree. The file manager
    // refuses a folder that would take a deck's slug, which is where the
    // collision can actually be created.
    let _ = check_deck_slug_collisions;

    let state = AppState {
        config: config.clone(),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        custom_decks: Arc::new(Mutex::new(custom_decks)),
        config_path,
        interrupted_closed: Arc::new(Mutex::new(interrupted_closed)),
        migration_failures: Arc::new(migration_failures),
        session_key: session_key(config.oidc.as_ref())?,
        oidc,
    };

    spawn_session_eviction_task(state.sessions.clone(), config.session_timeout_minutes);

    let app = Router::new()
        .route("/", get(landing_handler))
        .route("/files", get(files_get_handler))
        .route("/files/folder", post(files_folder_handler))
        .route("/files/file", post(files_file_handler))
        .route("/files/rename", post(files_rename_handler))
        .route("/files/delete", post(files_delete_handler))
        .route("/files/edit/{*path}", get(editor_get_handler))
        .route("/files/edit/{*path}", post(editor_post_handler))
        .route("/files/preview", post(preview_handler))
        // The body is the image itself, so this one route needs a limit
        // wider than axum's 2 MB default — and no wider than the one the
        // editor promises.
        .route(
            "/files/media/{*path}",
            post(media_upload_handler).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route("/decks", get(decks_manage_handler))
        .route("/decks/add", post(deck_add_handler))
        .route("/decks/delete", post(deck_delete_handler))
        .route("/collection/{slug}", get(collection_get_handler))
        .route("/collection/{slug}", post(collection_post_handler))
        .route("/collection/{slug}/start", post(collection_start_handler))
        .route("/collection/{slug}/stats", get(collection_stats_handler))
        .route("/collection/{slug}/export", get(collection_export_handler))
        .route("/collection/{slug}/bookmarks", get(bookmark_list_handler))
        .route(
            "/collection/{slug}/bookmarks/{hash}/delete",
            post(bookmark_delete_handler),
        )
        .route(
            "/collection/{slug}/bookmarks/{hash}/note",
            post(bookmark_note_handler),
        )
        .route("/collection/{slug}/edit/{hash}", get(edit_get_handler))
        .route("/collection/{slug}/edit/{hash}", post(edit_post_handler))
        .route(
            "/collection/{slug}/file/{*path}",
            get(collection_file_handler),
        )
        .route(
            "/collection/{slug}/script.js",
            get(collection_script_handler),
        );

    // The stylesheet, its fonts and the vendored libraries hold nothing about
    // anyone's cards, and every page that is shown to a logged-out user — the
    // login page, the "session expired" page — links them. Behind the gate
    // those links redirect to `/auth/login`, so exactly the pages a user meets
    // before they have a session were the pages that arrived unstyled.
    let static_routes = Router::new()
        .route("/manifest.json", get(manifest_handler))
        .route("/icons/icon-192.png", get(icon_192_handler))
        .route("/icons/icon-512.png", get(icon_512_handler))
        .route("/script.js", get(script_handler))
        // Every asset below is served `immutable` under a revision that
        // names this build's copy of it, and the handlers accept *any*
        // revision: a client working from HTML older than the running build
        // asks for a revision that no longer exists, and must be answered
        // with the current bytes rather than a 404. An unstyled page, or one
        // with no maths, is worse than a stale one.
        .route("/style/{rev}/style.css", get(style_handler))
        // The fixed paths every build before revisioning rendered into its
        // HTML. Kept for as long as such a page can still be in a cache.
        .route("/style.css", get(legacy_style_handler))
        .route("/fonts/{rev}/{name}", get(font_handler))
        .route("/fonts/{name}", get(legacy_font_handler))
        .route("/katex/{rev}/katex.css", get(katex_css_handler))
        .route("/katex/{rev}/katex.js", get(katex_js_handler))
        .route("/katex/{rev}/mhchem.js", get(katex_mhchem_js_handler))
        .route("/katex/fonts/{rev}/{name}", get(katex_font_handler))
        .route("/katex/katex.css", get(legacy_katex_css_handler))
        .route("/katex/katex.js", get(legacy_katex_js_handler))
        .route("/katex/mhchem.js", get(legacy_katex_mhchem_js_handler))
        .route("/katex/fonts/{name}", get(legacy_katex_font_handler))
        .route("/hljs/{rev}/github.css", get(hljs_css_handler))
        .route("/hljs/{rev}/highlight.js", get(hljs_js_handler))
        .route("/hljs/github.css", get(legacy_hljs_css_handler))
        .route("/hljs/highlight.js", get(legacy_hljs_js_handler));

    // `/auth/*` must NOT go through `require_auth` — gating the login route
    // itself behind login is the classic OIDC redirect loop.
    let app = if state.oidc.is_some() {
        let auth_routes = Router::new()
            .route("/auth/login", get(login_handler))
            .route("/auth/callback", get(callback_handler))
            // POST, not GET: a GET logout is triggerable by any third-party
            // page embedding it as an image.
            .route("/auth/logout", post(logout_handler));
        app.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ))
        .merge(auth_routes)
    } else {
        app
    };
    let app = app.merge(static_routes);
    let app = app.with_state(state);

    log::debug!("Starting server on {bind}");
    let listener = TcpListener::bind(&bind).await?;
    println!("hashcards-web running on http://{bind}/");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    log::debug!("Server shut down");
    Ok(())
}

/// Served from a fixed path — the drill's copy of it is per collection, so
/// the two cannot share one hashed name — and therefore never cached without
/// asking first. A script a week out of step with the HTML that loads it is
/// the same bug as a stale stylesheet, quieter.
async fn script_handler() -> (
    axum::http::StatusCode,
    [(axum::http::HeaderName, &'static str); 2],
    &'static str,
) {
    (
        axum::http::StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "text/javascript"),
            (
                axum::http::header::CACHE_CONTROL,
                crate::utils::CACHE_CONTROL_REVALIDATE,
            ),
        ],
        // Landing/browse pages use this route and expect MACROS to be defined.
        concat!("let MACROS = {};\n\n", include_str!("../drill/script.js")),
    )
}

type StaticResponse = (
    axum::http::StatusCode,
    [(axum::http::HeaderName, &'static str); 2],
    &'static [u8],
);

fn stylesheet(cache_control: &'static str) -> StaticResponse {
    (
        axum::http::StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "text/css"),
            (axum::http::header::CACHE_CONTROL, cache_control),
        ],
        STYLE_CSS.as_bytes(),
    )
}

/// The stylesheet at its content-addressed path. `immutable` is honest at
/// this build's revision: the bytes cannot change without the path changing
/// with them. At any other revision the request comes from HTML this build
/// did not render, so it is answered with the current stylesheet — which is
/// the one that page's successor will ask for — but may not be kept.
async fn style_handler(axum::extract::Path(rev): axum::extract::Path<String>) -> StaticResponse {
    stylesheet(crate::utils::revisioned_cache_control(&rev, &STYLE_REV))
}

/// The same bytes at the fixed path, for HTML that predates the hashed one.
/// Nothing may be cached against a name that does not describe its contents.
async fn legacy_style_handler() -> StaticResponse {
    stylesheet(crate::utils::CACHE_CONTROL_REVALIDATE)
}

async fn shutdown_signal() {
    terminate_signal().await;
    log::debug!("Received shutdown signal");
}

/// Periodically evict drill sessions idle past the configured timeout,
/// closing their DB session rows (BUG-08).
fn spawn_session_eviction_task(
    sessions: Arc<Mutex<HashMap<SessionKey, SharedSession>>>,
    timeout_minutes: u64,
) {
    if timeout_minutes == 0 {
        log::debug!("Idle session eviction disabled (session_timeout_minutes = 0)");
        return;
    }
    tokio::spawn(async move {
        // Check at a quarter of the timeout, capped at every 10 minutes.
        let tick_secs = (timeout_minutes * 60 / 4).clamp(1, 600);
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(tick_secs));
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let sessions = sessions.clone();
            match tokio::task::spawn_blocking(move || {
                evict_idle_sessions(&sessions, timeout_minutes, Timestamp::now())
            })
            .await
            {
                Ok(evicted) if !evicted.is_empty() => {
                    let slugs: Vec<&str> = evicted.iter().map(|k| k.slug()).collect();
                    log::info!(
                        "Evicted {} idle drill session(s): {}",
                        evicted.len(),
                        slugs.join(", ")
                    );
                }
                Ok(_) => {}
                Err(e) => log::error!("Session eviction task failed: {e}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::serve::cards::collection_id;
    use crate::helper::create_tmp_directory;
    use crate::user_db::UserDatabase;

    /// Two users may each have a collection called "Spanish". They slugify
    /// alike, so a notice keyed by slug would be shown to whichever of them
    /// opened the page first, reporting the other's interrupted sessions.
    /// Keyed by database path, each notice reaches its own owner.
    #[test]
    fn interrupted_notices_are_keyed_per_database_not_per_slug() -> Fallible<()> {
        let data_dir = create_tmp_directory()?;
        let db_dir = data_dir.join("db");
        ensure_dir(&db_dir, "review database directory")?;

        let mut db_paths = Vec::new();
        for user in ["alice-example.com", "bob-example.com"] {
            let folder = data_dir.join("cards").join(user).join("Spanish");
            std::fs::create_dir_all(&folder)?;
            let id = collection_id(&folder)?;
            let db_path = db_dir.join(format!("{user}.db"));
            let db = UserDatabase::open(&db_path)?.collection(id);
            // A session row opened long ago and never closed: exactly what a
            // crash leaves behind.
            let started = Timestamp::now().minus_minutes(SESSION_STALE_MINUTES * 2);
            db.create_session(started)?;
            db_paths.push(db_path);
        }

        let counts = sweep_dangling_sessions(&data_dir, &HashMap::new());
        assert_eq!(counts.len(), 2, "each user's collection swept separately");
        for db_path in &db_paths {
            assert_eq!(counts.get(db_path), Some(&1), "{}", db_path.display());
        }
        Ok(())
    }
}
