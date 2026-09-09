use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use parking_lot::Mutex;

use axum::Form;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderName;
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::http::header::CONTENT_TYPE;
use axum::response::Html;
use axum::response::Redirect;
use maud::html;

use crate::flash::Flash;
use crate::utils::CACHE_CONTROL_REVALIDATE;

use crate::cmd::drill::cache::Cache;
use crate::cmd::drill::get::RenderContext;
use crate::cmd::drill::get::render_completion_page;
use crate::cmd::drill::get::render_session_page;
use crate::cmd::drill::post::Action;
use crate::cmd::drill::post::ActionResult;
use crate::cmd::drill::post::FormData;
use crate::cmd::drill::post::handle_action;
use crate::cmd::drill::render::render_macros_declaration;
use crate::cmd::drill::state::MutableState;
use crate::cmd::drill::state::SessionDb;
use crate::cmd::drill::state::SessionDbs;
use crate::cmd::drill::state::SessionSource;
use crate::cmd::drill::template::page_template;
use crate::cmd::drill::template::page_template_with_script;
use crate::cmd::run_blocking;
use crate::cmd::serve::auth::CurrentUser;
use crate::cmd::serve::browse::build_deck_tree;
use crate::cmd::serve::browse::render_browse_page;
use crate::cmd::serve::config::ResolvedCollection;
use crate::cmd::serve::decks::ResolvedCustomDeck;
use crate::cmd::serve::decks::find_custom_deck;
use crate::cmd::serve::files::existing_collections_for_user;
use crate::cmd::serve::reviewdb::open_collection_db;
use crate::cmd::serve::state::AppState;
use crate::cmd::serve::state::DrillSession;
use crate::cmd::serve::state::SessionKey;
use crate::cmd::serve::state::SharedSession;
use crate::collection::Collection;
use crate::error::ErrorReport;
use crate::error::Fallible;
use crate::error::fail;
use crate::media::load::MediaLoader;
use crate::rng::TinyRng;
use crate::rng::shuffle;
use crate::types::card::Card;
use crate::types::card_hash::CardHash;
use crate::types::date::Date;
use crate::types::timestamp::Timestamp;
use crate::user_db::UserDatabase;

/// Run blocking filesystem/SQLite work on tokio's blocking thread pool.
///
/// Serve handlers parse collections from disk and touch SQLite; doing that
/// on the async executor stalls every other request (BUG-44). Pattern as in
/// the counting paths (`spawn_blocking` in the landing handler).
pub async fn collection_get_handler(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    current_user: Option<CurrentUser>,
) -> (StatusCode, Html<String>) {
    let flash = Flash::from_query(&query);
    let owner = current_user.map(|u| u.email);
    // A slug that isn't the caller's (wrong owner, or doesn't exist at all)
    // 404s before touching the session map, so an active session belonging
    // to a different owner is never reachable by slug alone.
    if find_drill_target(&state, &slug, owner.as_deref()).is_none() {
        return (StatusCode::NOT_FOUND, collection_not_found_page());
    }
    let state2 = state.clone();
    let slug2 = slug.clone();
    let owner2 = owner.clone();
    match run_blocking(move || collection_get_inner(&state2, &slug2, flash, owner2.as_deref()))
        .await
    {
        Ok(html) => (StatusCode::OK, Html(html)),
        Err(e) => {
            let html = page_template(html! {
                div.error {
                    h1 { "Error" }
                    p { (e) }
                    a href="/" { "Back to collections" }
                }
            })
            .into_string();
            (StatusCode::INTERNAL_SERVER_ERROR, Html(html))
        }
    }
}

fn collection_not_found_page() -> Html<String> {
    Html(
        page_template(html! {
            div.error {
                h1 { "Error" }
                p { "Unknown collection" }
                a href="/" { "Back to collections" }
            }
        })
        .into_string(),
    )
}

fn collection_get_inner(
    state: &AppState,
    slug: &str,
    flash: Option<Flash>,
    owner: Option<&str>,
) -> Fallible<String> {
    // Clone the Arc out of the map so the map lock is not held during
    // rendering; the session itself stays in the map even if rendering fails.
    let key = SessionKey::new(owner, slug);
    let session: Option<SharedSession> = state.sessions.lock().get(&key).cloned();

    // A concurrent Home action (or eviction) may have removed this session
    // from the map, and closed its DB row, between the clone above and the
    // lock below; `is_detached` is how removers report that (see the
    // `detached` field's doc comment). Treat it exactly like "no session in
    // the map" rather than rendering a stale session or touching its row.
    let session = session.filter(|s| !s.lock().is_detached());

    let Some(session) = session else {
        // A custom deck has a fixed membership, so it gets a start page
        // rather than the collection's deck tree.
        if let Some(deck) = find_custom_deck(&state.custom_decks.lock(), slug, owner) {
            let sources = deck_sources(state, &deck, owner);
            let due = due_card_count(state, &sources)?;
            return Ok(render_custom_deck_page(&deck, &sources, due, flash).into_string());
        }
        // No active session: show the deck browser.
        let rc = find_collection(state, slug, owner)
            .ok_or_else(|| crate::error::ErrorReport::new(format!("Unknown collection: {slug}")))?;
        // Two views, because `build_deck_tree` consumes the one it is
        // given. They share the user's connection, so this is not a second
        // writer on the file.
        let browse = build_deck_tree(&rc.coll_dir, open_collection_db(state, &rc)?)?;
        let db = open_collection_db(state, &rc)?;
        // FEAT-03: report the session rows the startup sweep closed. The
        // sweep itself runs once, at startup: it cannot tell a crashed
        // session from a live one, and a second server may share the same
        // database, so doing it per request would stamp `ended_at` on a
        // session that is still going. Taking the entry also means the
        // notice appears once instead of on every visit.
        let interrupted_closed = state
            .interrupted_closed
            .lock()
            .remove(&rc.db_path)
            .unwrap_or(0);
        let bookmark_count = db.count_bookmarks()?;
        let html = render_browse_page(
            &rc.name,
            slug,
            &browse,
            bookmark_count,
            interrupted_closed,
            flash,
        );
        return Ok(html.into_string());
    };

    let mut session = session.lock();
    // BUG-08: stamp activity so the eviction task only reaps idle sessions.
    session.last_activity_at = Timestamp::now();
    // BUG-14: start the per-card timer when the card is served.
    session.mutable.mark_card_shown();
    // Heartbeat, so another process's startup sweep can tell this session
    // apart from one abandoned by a crash.
    if let Err(e) = session.mutable.dbs.touch_all(Timestamp::now()) {
        log::debug!("could not stamp session heartbeat: {e}");
    }
    let form_action = format!("/collection/{slug}");
    let file_url_prefix = format!("/collection/{slug}/file");
    let ctx = RenderContext {
        directory: &session.directory,
        total_cards: session.total_cards,
        session_started_at: session.session_started_at,
        answer_controls: session.answer_controls,
        form_action: &form_action,
        file_url_prefix: &file_url_prefix,
        slug,
    };
    let body = if session.mutable.finished_at.is_some() {
        render_completion_page(&ctx, &session.mutable)?
    } else {
        render_session_page(&ctx, &session.mutable)?
    };
    let body = html! {
        @if let Some(f) = &flash { (f.render()) }
        (body)
    };
    let script_url = format!("/collection/{slug}/script.js");
    let html = page_template_with_script(&script_url, body);
    Ok(html.into_string())
}

/// Every slug that can be drilled: a collection or a user-assembled
/// cross-collection deck.
pub(super) enum DrillTarget {
    Collection(ResolvedCollection),
    Deck(ResolvedCustomDeck),
}

/// Resolve a drillable slug, scoped to `owner`. Collections win over decks;
/// startup validation refuses a config where the two could collide.
pub(super) fn find_drill_target(
    state: &AppState,
    slug: &str,
    owner: Option<&str>,
) -> Option<DrillTarget> {
    if let Some(rc) = find_collection(state, slug, owner) {
        return Some(DrillTarget::Collection(rc));
    }
    let decks = state.custom_decks.lock();
    find_custom_deck(&decks, slug, owner).map(DrillTarget::Deck)
}

/// The collections a custom deck draws on, each with the decks it wants.
///
/// Members naming a collection the caller no longer owns (renamed, removed,
/// or never theirs) are skipped rather than failing the whole deck: a stale
/// member should not make the rest of a deck undrillable.
pub(super) fn deck_sources(
    state: &AppState,
    deck: &ResolvedCustomDeck,
    owner: Option<&str>,
) -> Vec<SessionSourceSpec> {
    let mut by_collection: Vec<SessionSourceSpec> = Vec::new();
    for member in &deck.members {
        let Some(rc) = find_collection(state, &member.collection_slug, owner) else {
            continue;
        };
        match by_collection
            .iter_mut()
            .find(|s| s.collection.slug == rc.slug)
        {
            Some(existing) => existing.decks.push(member.deck_name.clone()),
            None => by_collection.push(SessionSourceSpec {
                collection: rc,
                decks: vec![member.deck_name.clone()],
            }),
        }
    }
    by_collection
}

/// Looks up a collection by slug, scoped to `owner` (the caller's email, or
/// `None` when `[oidc]` is off). A collection whose `owner` doesn't match is
/// treated exactly like a nonexistent one, so callers can 404 either case
/// identically rather than leaking which slugs exist for other users.
pub(super) fn find_collection(
    state: &AppState,
    slug: &str,
    owner: Option<&str>,
) -> Option<ResolvedCollection> {
    existing_collections_for_user(state, current_user_for(owner).as_ref())
        .into_iter()
        .find(|c| c.slug == slug && c.owner.as_deref() == owner)
}

/// `find_collection`, off the async executor.
///
/// The lookup is not free any more: local collections are discovered by
/// reading the caller's card folder and each folder's `.hashcards.toml`, so
/// calling it directly in an async handler blocks the executor — once per
/// request, and `collection_file_handler` serves one request per image on a
/// card.
pub(super) async fn find_collection_blocking(
    state: &AppState,
    slug: &str,
    owner: Option<String>,
) -> Option<ResolvedCollection> {
    let state = state.clone();
    let slug = slug.to_string();
    match tokio::task::spawn_blocking(move || find_collection(&state, &slug, owner.as_deref()))
        .await
    {
        Ok(found) => found,
        Err(e) => {
            log::error!("Collection lookup failed: {e}");
            None
        }
    }
}

/// Whether `slug` names a collection the caller can see. Used to tell "no
/// such collection" apart from "loading it failed", which is the only thing
/// the answer is needed for.
pub(super) async fn collection_exists(state: &AppState, slug: &str, owner: Option<String>) -> bool {
    find_collection_blocking(state, slug, owner).await.is_some()
}

/// `collections_for_user` keys the tree off a `CurrentUser`; callers here
/// already reduced that to an owner email.
pub(super) fn current_user_for(owner: Option<&str>) -> Option<CurrentUser> {
    owner.map(|email| CurrentUser {
        email: email.to_string(),
    })
}

/// Form data for the start-drill endpoint.
pub struct StartDrillForm {
    pub decks: Vec<String>,
    /// Optional card limit: `0` or absent means "all due cards".
    pub limit: Option<usize>,
    /// Set by the one-tap Drill button on the collection list, which offers
    /// no topic checkboxes at all. It is what tells an empty `decks` apart
    /// from a topic picker with everything unticked.
    pub all_topics: bool,
}

/// Custom `Deserialize` for `StartDrillForm`.
///
/// `serde_urlencoded` presents repeated keys (`decks=foo&decks=bar`) as
/// separate map entries rather than grouping them into a sequence first.
/// The derived `Deserialize` macro errors on "duplicate field" in that case,
/// so we implement the visitor manually to accumulate all `decks` values.
impl<'de> serde::Deserialize<'de> for StartDrillForm {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{MapAccess, Visitor};

        struct FormVisitor;

        impl<'de> Visitor<'de> for FormVisitor {
            type Value = StartDrillForm;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a form with optional decks and limit fields")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut decks = Vec::new();
                let mut limit: Option<usize> = None;
                let mut all_topics = false;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "decks" => decks.push(map.next_value::<String>()?),
                        "all_topics" => {
                            let _ = map.next_value::<String>()?;
                            all_topics = true;
                        }
                        "limit" => {
                            let raw = map.next_value::<String>()?;
                            if let Ok(n) = raw.parse::<usize>() {
                                if n > 0 {
                                    limit = Some(n);
                                }
                            }
                        }
                        _ => {
                            let _ = map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(StartDrillForm {
                    decks,
                    limit,
                    all_topics,
                })
            }
        }

        deserializer.deserialize_map(FormVisitor)
    }
}

pub async fn collection_start_handler(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    current_user: Option<CurrentUser>,
    Form(form): Form<StartDrillForm>,
) -> Redirect {
    let state2 = state.clone();
    let slug2 = slug.clone();
    let owner = current_user.map(|u| u.email);
    match run_blocking(move || {
        collection_start_inner(
            &state2,
            &slug2,
            form.decks,
            form.limit,
            form.all_topics,
            owner.as_deref(),
        )
    })
    .await
    {
        Ok(()) => Redirect::to(&format!("/collection/{slug}")),
        Err(e) => {
            log::error!("error starting drill for collection {slug}: {e}");
            Flash::error(e.to_string()).redirect(&format!("/collection/{slug}"))
        }
    }
}

fn collection_start_inner(
    state: &AppState,
    slug: &str,
    selected_decks: Vec<String>,
    limit: Option<usize>,
    all_topics: bool,
    owner: Option<&str>,
) -> Fallible<()> {
    // The slug must be the caller's own before anything else happens —
    // including before touching an existing session under that slug, which
    // could otherwise belong to a different owner.
    let target = find_drill_target(state, slug, owner)
        .ok_or_else(|| ErrorReport::new(format!("Unknown collection: {slug}")))?;
    // A custom deck's membership is fixed when it is saved, so it carries no
    // topic checkboxes; a collection needs at least one — unless the caller
    // asked for all of them, which an empty `decks` already means downstream.
    if matches!(target, DrillTarget::Collection(_)) && selected_decks.is_empty() && !all_topics {
        return fail("Select at least one topic.");
    }
    // FEAT-03: never silently discard an unfinished session. The redirect
    // to /collection/{slug} lands on the running session; the user must
    // End it (or let BUG-08 eviction reap it) before starting a new one.
    let key = SessionKey::new(owner, slug);
    {
        let mut sessions = state.sessions.lock();
        if let Some(existing) = sessions.get(&key) {
            if existing.lock().mutable.finished_at.is_none() {
                return Ok(());
            }
        }
        // Finished sessions are replaced.
        if let Some(previous) = sessions.remove(&key) {
            previous.lock().detach();
        }
    }

    // Create the session outside the lock (may do filesystem/DB work).
    let session = match target {
        DrillTarget::Collection(rc) => create_session_from_sources(
            state,
            vec![SessionSourceSpec {
                collection: rc,
                decks: selected_decks,
            }],
            limit,
        )?,
        DrillTarget::Deck(deck) => {
            let sources = deck_sources(state, &deck, owner);
            if sources.is_empty() {
                return fail(format!(
                    "The deck '{}' has no members in collections you own. Edit it from the \
                     Decks page.",
                    deck.name
                ));
            }
            create_session_from_sources(state, sources, limit)?
        }
    };
    if let Some(s) = session {
        state.sessions.lock().insert(key, Arc::new(Mutex::new(s)));
    }
    Ok(())
}

/// One collection contributing to a drill session, and which of its decks
/// are wanted. An empty `decks` means every deck in the collection.
pub(super) struct SessionSourceSpec {
    pub collection: ResolvedCollection,
    pub decks: Vec<String>,
}

/// Build a drill session over one or more collections.
///
/// Each source keeps its own database and its own open session row, and every
/// card is routed back to the collection it came from, so a card drilled
/// inside a cross-collection deck is scheduled exactly once — in its home
/// collection — rather than acquiring a second, drifting schedule.
pub(super) fn create_session_from_sources(
    state: &AppState,
    sources: Vec<SessionSourceSpec>,
    limit: Option<usize>,
) -> Fallible<Option<DrillSession>> {
    if sources.is_empty() {
        return Ok(None);
    }
    let multi_source = sources.len() > 1;
    // Checked once at startup too; resolved here because a collection may
    // override it and a deck may draw on several that disagree.
    let defaults = state.config.defaults.scheduling()?;
    let session_started_at = Timestamp::now();
    let today: Date = session_started_at.date();

    // Every collection a deck can draw on belongs to one user, so they all
    // name one file — but keyed by path rather than assumed, so a deck that
    // one day spanned two users would still be correct rather than silently
    // routed to the wrong database.
    let mut opened: HashMap<PathBuf, UserDatabase> = HashMap::new();
    let mut session_dbs: Vec<SessionDb> = Vec::new();
    let mut routes: HashMap<CardHash, usize> = HashMap::new();
    let mut due_cards: Vec<Card> = Vec::new();
    let mut cache = Cache::new();
    let mut first_directory: Option<PathBuf> = None;
    let mut macros: Vec<(String, String)> = Vec::new();

    for (index, spec) in sources.into_iter().enumerate() {
        let rc = spec.collection;
        let user_db = match opened.entry(rc.db_path.clone()) {
            Entry::Occupied(slot) => slot.into_mut(),
            Entry::Vacant(slot) => slot.insert(UserDatabase::open(&rc.db_path)?),
        };
        let collection = Collection::open(
            rc.coll_dir.clone(),
            user_db.collection(rc.collection_id.clone()),
        )?;
        // Canonical, as every card's file path is: media is resolved by
        // stripping this prefix off the file the card was parsed from.
        let coll_dir = collection.directory.clone();

        // Sync new cards to DB
        let db_hashes: HashSet<CardHash> = collection.db.card_hashes()?;
        for card in collection.cards.iter() {
            if !db_hashes.contains(&card.hash()) {
                collection.db.insert_card(card.hash(), session_started_at)?;
            }
        }

        // Filter by selected decks.
        let deck_filter: HashSet<&str> = spec.decks.iter().map(|s| s.as_str()).collect();
        let cards: Vec<Card> = if deck_filter.is_empty() {
            collection.cards
        } else {
            collection
                .cards
                .into_iter()
                .filter(|card| deck_filter.contains(card.deck_name().as_str()))
                .collect()
        };

        // Find cards due today
        let due_today: HashSet<CardHash> = collection.db.due_today(today)?;
        for card in cards.into_iter().filter(|c| due_today.contains(&c.hash())) {
            // Cards are content addressed, so the same fact written into two
            // collections has one hash. Drill it once, routed to the first
            // collection that offered it; the other collection's own copy
            // stays due there and is reviewed when that collection is
            // drilled. Without this the session cache would reject the
            // second copy outright and the whole deck would fail to start.
            if routes.contains_key(&card.hash()) {
                continue;
            }
            let performance = collection.db.get_card_performance(card.hash())?;
            cache.insert(card.hash(), performance)?;
            routes.insert(card.hash(), index);
            due_cards.push(card);
        }

        // Open this collection's session row immediately, so reviews can be
        // written as they happen.
        let session_id = collection.db.create_session(session_started_at)?;
        session_dbs.push(SessionDb {
            db: collection.db,
            session_id,
            // Recorded for every session, not only a multi-collection one:
            // a card's media is served from the collection that holds it,
            // and a deck's slug names no collection, so a session addressed
            // by one has no usable prefix of its own.
            source: Some(SessionSource {
                coll_dir,
                file_url_prefix: format!("/collection/{}/file", rc.slug),
            }),
            // The collection's own settings where it has them, the
            // instance's elsewhere. Resolved per database, so a deck
            // spanning collections schedules each card by the collection it
            // came from.
            scheduling: rc.scheduling(defaults),
        });
        if first_directory.is_none() {
            first_directory = Some(collection.directory);
            macros = collection.macros;
        }
    }

    if state.config.defaults.bury_siblings {
        due_cards = bury_siblings(due_cards);
    }

    if due_cards.is_empty() {
        // The session rows opened above would otherwise linger as dangling
        // "interrupted" sessions with no reviews.
        for entry in &session_dbs {
            entry
                .db
                .close_session(entry.session_id, session_started_at)?;
        }
        return Ok(None);
    }

    // Seed a session RNG, used for shuffling and interval jitter.
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| {
            ErrorReport::new(format!(
                "The system clock is set before the Unix epoch: {e}"
            ))
        })?
        .as_nanos() as u64;
    let mut rng = TinyRng::from_seed(seed);
    due_cards = shuffle(due_cards, &mut rng);

    // Apply session card limit if requested.
    if let Some(n) = limit {
        due_cards.truncate(n);
    }

    let answer_controls = state.config.defaults.answer_controls.into();
    let directory = first_directory
        .ok_or_else(|| ErrorReport::new("a drill session needs at least one collection"))?;
    let dbs = if multi_source {
        SessionDbs::routed(session_dbs, routes)
    } else {
        // One collection: every card belongs to it, so no routing table.
        SessionDbs::routed(session_dbs, HashMap::new())
    };

    Ok(Some(DrillSession::new(
        directory,
        macros,
        session_started_at,
        answer_controls,
        MutableState::new(dbs, cache, due_cards, rng),
    )))
}

/// How many of a custom deck's cards are due today, across every collection
/// it draws on.
fn due_card_count(state: &AppState, sources: &[SessionSourceSpec]) -> Fallible<usize> {
    Ok(deck_card_counts(state, sources)?.0)
}

/// `(due today, total)` over the topics a custom deck names.
///
/// A deck is a selection, not a collection, so its counts cannot be cached
/// alongside the collection counts: they are recomputed from the member
/// collections each time they are shown.
pub(super) fn deck_card_counts(
    state: &AppState,
    sources: &[SessionSourceSpec],
) -> Fallible<(usize, usize)> {
    let today = Timestamp::now().date();
    let mut due_total = 0;
    let mut card_total = 0;
    for spec in sources {
        let collection = Collection::open(
            spec.collection.coll_dir.clone(),
            open_collection_db(state, &spec.collection)?,
        )?;
        let due: HashSet<CardHash> = collection.db.due_today(today)?;
        let wanted: HashSet<&str> = spec.decks.iter().map(|d| d.as_str()).collect();
        for card in collection
            .cards
            .iter()
            .filter(|c| wanted.contains(c.deck_name().as_str()))
        {
            card_total += 1;
            if due.contains(&card.hash()) {
                due_total += 1;
            }
        }
    }
    Ok((due_total, card_total))
}

/// The start page for a custom deck: what it contains, how much is due, and
/// a button to drill it.
fn render_custom_deck_page(
    deck: &ResolvedCustomDeck,
    sources: &[SessionSourceSpec],
    due_today: usize,
    flash: Option<Flash>,
) -> maud::Markup {
    // Saturating: a member can only ever resolve to at most one entry, but
    // the page must not panic if that ever stops holding.
    let missing = deck
        .members
        .len()
        .saturating_sub(sources.iter().map(|s| s.decks.len()).sum::<usize>());
    page_template(html! {
        @if let Some(f) = &flash { (f.render()) }
        div.landing {
            h1 { (deck.name) }
            p { a href="/" { "\u{2190} Back to collections" } }
            p.empty {
                (format!("{due_today} card(s) due today across {} collection(s).", sources.len()))
            }
            @if missing > 0 {
                div.notice {
                    p {
                        (format!(
                            "{missing} member(s) of this deck point at a collection that is no \
                             longer available, and are skipped."
                        ))
                    }
                }
            }
            @if sources.is_empty() {
                p.empty { "This deck has no members in collections you own." }
            } @else {
                table.collection-table {
                    thead { tr { th { "Collection" } th { "Topics" } } }
                    tbody {
                        @for source in sources {
                            tr {
                                td { (source.collection.name) }
                                td { (source.decks.join(", ")) }
                            }
                        }
                    }
                }
                @if due_today > 0 {
                    form action=(format!("/collection/{}/start", deck.slug)) method="post" {
                        label for="limit" { "Card limit (optional): " }
                        input id="limit" type="number" name="limit" min="1" placeholder="all";
                        input .btn.btn-primary type="submit" value="Drill";
                    }
                } @else {
                    p.empty { "Nothing due in this deck today." }
                }
            }
        }
    })
}

fn bury_siblings(deck: Vec<Card>) -> Vec<Card> {
    let mut seen_families = HashSet::new();
    let mut result = Vec::new();
    for card in deck.into_iter() {
        if let Some(family) = card.family_hash() {
            if seen_families.contains(&family) {
                continue;
            }
            seen_families.insert(family);
        }
        result.push(card);
    }
    result
}

pub async fn collection_post_handler(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    current_user: Option<CurrentUser>,
    Form(form): Form<FormData>,
) -> Redirect {
    let state2 = state.clone();
    let slug2 = slug.clone();
    let owner = current_user.map(|u| u.email);
    match run_blocking(move || collection_post_inner(&state2, &slug2, form, owner.as_deref())).await
    {
        Ok(redirect) => redirect,
        Err(e) => {
            log::error!("error handling action for collection {slug}: {e}");
            Flash::error(e.to_string()).redirect(&format!("/collection/{slug}"))
        }
    }
}

fn collection_post_inner(
    state: &AppState,
    slug: &str,
    form: FormData,
    owner: Option<&str>,
) -> Fallible<Redirect> {
    // A grading/Home action on a slug that isn't the caller's own is refused
    // before the session map is touched at all. The map is keyed by owner as
    // well as slug, so this cannot reach someone else's session either way;
    // refusing here is what turns "not yours" into an error rather than a
    // silent no-op against an empty key.
    if find_drill_target(state, slug, owner).is_none() {
        return fail(format!("Unknown collection: {slug}"));
    }
    let key = SessionKey::new(owner, slug);
    let action = form.action;
    let submitted_undo: Option<i64> = form.undo_review;
    let submitted_card: Option<CardHash> = match form.card.as_deref() {
        Some(hex) => match CardHash::from_hex(hex) {
            Ok(hash) => Some(hash),
            Err(_) => {
                log::debug!("ignoring grade with unparseable card hash for collection {slug}");
                return Ok(Flash::error(
                    "That grade carried an invalid card reference and was ignored.",
                )
                .redirect(&format!("/collection/{slug}")));
            }
        },
        None => None,
    };
    // Home action: close the session and drop it without needing to hold the
    // global lock during DB work.
    if matches!(action, Action::Home) {
        let session = state.sessions.lock().remove(&key);
        if let Some(s) = session {
            let mut s = s.lock();
            s.detach();
            if s.mutable.finished_at.is_none() {
                if let Err(e) = s.mutable.dbs.close_all(Timestamp::now()) {
                    log::error!("failed to close the session for collection {slug}: {e}");
                }
            }
        }

        return Ok(Redirect::to("/"));
    }

    // Lock the session in place: the map lock is released immediately, the
    // per-slug lock is held for the DB work, and an error leaves the session
    // in the map untouched.
    let session: SharedSession = match state.sessions.lock().get(&key).cloned() {
        Some(s) => s,
        None => return Ok(Redirect::to(&format!("/collection/{slug}"))),
    };
    let mut guard = session.lock();

    // Re-check that this is still the session the map holds for `slug`: a
    // concurrent Home request may have removed it (and closed its DB row)
    // while we were waiting for the session lock, a concurrent start may have
    // replaced it with a new drill, or the idle sweep may have evicted it.
    //
    // This must not re-read the sessions map: taking the map lock here, while
    // the session lock is held, inverts the global order (map, then session)
    // that `evict_idle_sessions` and the landing page follow, and two
    // `parking_lot` mutexes with no timeout deadlock the server outright.
    // Every removal marks the session instead, under the map lock, so the
    // flag is all we need.
    if guard.is_detached() {
        return Ok(
            Flash::error("The session has ended, so that action was ignored.")
                .redirect(&format!("/collection/{slug}")),
        );
    }

    let session = &mut *guard;
    // BUG-08: stamp activity so the eviction task only reaps idle sessions.
    session.last_activity_at = Timestamp::now();

    // `Action::Home` returned early above, and it is the only action for which
    // `handle_action` yields `ActionResult::Home`. Every action reaching here
    // leaves the session running; the only result needing dispatch is
    // `Ignored`, which carries a one-shot message for the user.
    let result = handle_action(&mut session.mutable, action, submitted_card, submitted_undo)?;

    match result {
        ActionResult::Ignored(reason) => {
            Ok(Flash::error(reason).redirect(&format!("/collection/{slug}")))
        }
        _ => Ok(Redirect::to(&format!("/collection/{slug}"))),
    }
}

/// The content type a media file is served as, from its extension.
///
/// Every format `upload::sniff_image` will store must have an arm here: a
/// file served as `application/octet-stream` is offered as a download
/// rather than drawn in the card that references it.
fn content_type_for(extension: &str) -> &'static str {
    match extension {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
}

pub async fn collection_file_handler(
    State(state): State<AppState>,
    Path((slug, path)): Path<(String, String)>,
    current_user: Option<CurrentUser>,
) -> (StatusCode, [(HeaderName, &'static str); 1], Vec<u8>) {
    let owner = current_user.map(|u| u.email);
    let coll_dir = match find_collection_blocking(&state, &slug, owner).await {
        Some(rc) => rc.coll_dir.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                [(CONTENT_TYPE, "text/plain")],
                b"Collection not found".to_vec(),
            );
        }
    };

    let loader = match MediaLoader::new(coll_dir) {
        Ok(loader) => loader,
        Err(error) => {
            log::error!("Failed to create media loader: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(CONTENT_TYPE, "text/plain")],
                b"Internal Server Error".to_vec(),
            );
        }
    };
    let validated_path: PathBuf = match loader.validate(&path) {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                [(CONTENT_TYPE, "text/plain")],
                b"Not Found".to_vec(),
            );
        }
    };
    let extension = validated_path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_lowercase();
    let content_type: &str = content_type_for(&extension);
    let content = tokio::fs::read(validated_path).await;
    match content {
        Ok(bytes) => (StatusCode::OK, [(CONTENT_TYPE, content_type)], bytes),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(CONTENT_TYPE, "text/plain")],
            b"Internal Server Error".to_vec(),
        ),
    }
}

/// The drill's script, with the collection's LaTeX macros declared ahead of
/// it. Its contents depend on the session, so its path cannot name them: it
/// must be revalidated rather than cached against a slug that stays put while
/// the macros under it move.
pub async fn collection_script_handler(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    current_user: Option<CurrentUser>,
) -> (StatusCode, [(HeaderName, &'static str); 2], String) {
    let owner = current_user.map(|u| u.email);
    let script = |macros: &[(String, String)]| {
        format!(
            "{}\n{}",
            render_macros_declaration(macros),
            include_str!("../drill/script.js")
        )
    };
    if find_drill_target(&state, &slug, owner.as_deref()).is_none() {
        return (
            StatusCode::OK,
            [
                (CONTENT_TYPE, "text/javascript"),
                (CACHE_CONTROL, CACHE_CONTROL_REVALIDATE),
            ],
            script(&[]),
        );
    }
    let key = SessionKey::new(owner.as_deref(), &slug);
    let session: Option<SharedSession> = state.sessions.lock().get(&key).cloned();
    let macros: Vec<(String, String)> = match session {
        Some(session) => session.lock().macros.clone(),
        // No active session; serve the script without macros.
        None => Vec::new(),
    };
    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, "text/javascript"),
            (CACHE_CONTROL, CACHE_CONTROL_REVALIDATE),
        ],
        script(&macros),
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use std::time::Instant;

    use super::SessionSourceSpec;
    use super::collection_get_inner;
    use super::create_session_from_sources;
    use super::deck_sources;
    use super::run_blocking;
    use crate::cmd::drill::render::AnswerControls;
    use crate::cmd::drill::state::MutableState;
    use crate::cmd::drill::state::SessionDbs;
    use crate::cmd::serve::config::ResolvedCollection;
    use crate::cmd::serve::reviewdb::open_collection_db;
    use crate::cmd::serve::state::AppState;
    use crate::cmd::serve::state::DrillSession;
    use crate::cmd::serve::state::SessionKey;
    use crate::error::ErrorReport;
    use crate::error::Fallible;
    use crate::rng::TinyRng;
    use crate::types::collection_id::CollectionId;
    use crate::types::performance::Jitter;
    use crate::types::performance::Scheduling;
    use crate::types::timestamp::Timestamp;

    /// Every image format the paste path will store has to be served as
    /// an image: `sniff_image` accepts WebP, and a WebP served as
    /// `application/octet-stream` renders as a broken image in every card
    /// that references it.
    #[test]
    fn every_stored_image_format_is_served_as_an_image() {
        for bytes in [
            [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A].to_vec(),
            vec![0xFF, 0xD8, 0xFF, 0xE0],
            b"GIF89a".to_vec(),
            b"RIFF\0\0\0\0WEBPVP8 ".to_vec(),
        ] {
            let extension =
                crate::cmd::serve::upload::sniff_image(&bytes).expect("a supported format");
            let served = super::content_type_for(extension);
            assert!(
                served.starts_with("image/"),
                "`.{extension}` is served as `{served}`"
            );
        }
    }

    /// Cross-user isolation: `find_collection` only matches a collection
    /// whose `owner` equals the caller's, and unauthenticated (`None`)
    /// callers only match unowned collections.
    #[test]
    fn test_find_collection_is_scoped_to_owner() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let data_dir = dir.path().to_path_buf();
        card_collection(
            &data_dir,
            Some("alice@example.com"),
            "alice-deck",
            "Q: One\nA: 1\n",
        )?;
        card_collection(
            &data_dir,
            Some("bob@example.com"),
            "bob-deck",
            "Q: Two\nA: 2\n",
        )?;
        let state = crate::cmd::serve::state::test_support::state_with_data_dir(data_dir);

        assert!(super::find_collection(&state, "bob-deck", Some("alice@example.com")).is_none());
        assert!(super::find_collection(&state, "alice-deck", Some("alice@example.com")).is_some());
        assert!(super::find_collection(&state, "alice-deck", None).is_none());
        Ok(())
    }

    /// Create a collection folder named `name` in `owner`'s card tree under
    /// `data_dir`, holding one card, stamped with a stable id.
    fn card_collection(
        data_dir: &std::path::Path,
        owner: Option<&str>,
        name: &str,
        card: &str,
    ) -> Fallible<()> {
        use crate::cmd::serve::cards::CardRoot;
        use crate::cmd::serve::cards::collection_id;

        let root = CardRoot::for_user(data_dir, owner)?;
        let folder = root.path().join(name);
        std::fs::create_dir_all(&folder)?;
        std::fs::write(folder.join("Deck.md"), card)?;
        std::fs::create_dir_all(data_dir.join("db"))?;
        collection_id(&folder)?;
        Ok(())
    }

    /// A state whose card tree holds one collection named `Deck`, and that
    /// collection as discovery reports it.
    fn test_state(data_dir: &std::path::Path) -> Fallible<(AppState, ResolvedCollection)> {
        card_collection(data_dir, None, "Deck", "Q: One\nA: 1\n")?;
        let state =
            crate::cmd::serve::state::test_support::state_with_data_dir(data_dir.to_path_buf());
        let rc = super::find_collection(&state, "Deck", None)
            .ok_or_else(|| ErrorReport::new("the collection was not discovered"))?;
        Ok((state, rc))
    }

    /// Regression: a session removed from the map between the GET handler's
    /// clone and its lock (a concurrent Home action, or eviction) must not be
    /// rendered as if it were still live. `collection_post_inner` already
    /// guarded this with an `is_detached` check; `collection_get_inner` did
    /// not, so it would render the stale session and re-stamp its (already
    /// closed) DB row's heartbeat.
    #[test]
    fn test_detached_session_renders_browse_page_not_stale_session() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let data_dir = dir.path().canonicalize()?;

        let (state, rc) = test_state(&data_dir)?;
        let slug = rc.slug.clone();
        let started_at = Timestamp::now();
        let db = open_collection_db(&state, &rc)?;
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
            crate::cmd::drill::cache::Cache::new(),
            Vec::new(),
            TinyRng::from_seed(1),
        );
        let session = std::sync::Arc::new(parking_lot::Mutex::new(DrillSession::new(
            rc.coll_dir.clone(),
            Vec::new(),
            started_at,
            AnswerControls::Full,
            mutable,
        )));
        session.lock().detach();
        state
            .sessions
            .lock()
            .insert(SessionKey::new(None, &slug), session);

        let html = collection_get_inner(&state, &slug, None, None)?;
        assert!(
            html.contains(r#"class="browse""#),
            "detached session must fall back to the deck browser page, got: {html}"
        );
        Ok(())
    }

    /// Cross-user isolation: a collection is a folder in its owner's card
    /// tree, so two users can each own one named "Deck" and both slugify to
    /// `Deck`. Keyed by slug alone, the sessions map handed whoever asked
    /// second the other's live session — their card to read, and their
    /// database to grade into.
    #[test]
    fn a_session_is_not_shared_by_two_owners_of_the_same_slug() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let data_dir = dir.path().to_path_buf();
        card_collection(
            &data_dir,
            Some("alice@example.com"),
            "Deck",
            "Q: alice question?\nA: alice answer\n",
        )?;
        card_collection(
            &data_dir,
            Some("bob@example.com"),
            "Deck",
            "Q: bob question?\nA: bob answer\n",
        )?;
        let state = crate::cmd::serve::state::test_support::state_with_data_dir(data_dir);

        let alice = super::find_collection(&state, "Deck", Some("alice@example.com"))
            .ok_or_else(|| ErrorReport::new("alice's collection was not discovered"))?;
        let session = create_session_from_sources(
            &state,
            vec![SessionSourceSpec {
                collection: alice,
                decks: Vec::new(),
            }],
            None,
        )?
        .ok_or_else(|| ErrorReport::new("expected alice's new card to be due"))?;
        state.sessions.lock().insert(
            SessionKey::new(Some("alice@example.com"), "Deck"),
            std::sync::Arc::new(parking_lot::Mutex::new(session)),
        );

        let bob = collection_get_inner(&state, "Deck", None, Some("bob@example.com"))?;
        assert!(
            !bob.contains("alice question"),
            "bob was served alice's card: {bob}"
        );
        assert!(
            bob.contains(r#"class="browse""#),
            "bob must get his own topic browser, got: {bob}"
        );

        let alice = collection_get_inner(&state, "Deck", None, Some("alice@example.com"))?;
        assert!(
            alice.contains("alice question"),
            "alice must still reach her own session, got: {alice}"
        );
        Ok(())
    }

    /// Regression test (BUG-44): serve handlers used to run collection
    /// parsing and SQLite work inline on the async executor, so one slow
    /// request stalled every other request. `run_blocking` must move that
    /// work off the executor: on a single-threaded runtime, a concurrent
    /// timer still fires while the blocking closure sleeps.
    #[tokio::test(flavor = "current_thread")]
    async fn run_blocking_does_not_stall_the_executor() -> Fallible<()> {
        let started = Instant::now();
        let slow = tokio::spawn(run_blocking(|| {
            std::thread::sleep(Duration::from_millis(800));
            Ok(())
        }));

        // This timer shares the single executor thread with the task above.
        // It can only fire promptly if the sleep is not on that thread.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "the executor was blocked for {:?}",
            started.elapsed()
        );

        slow.await
            .map_err(|e| crate::error::ErrorReport::new(format!("join failed: {e}")))?
    }

    /// A custom deck drills cards from several collections in one session,
    /// and each review lands in the database of the collection the card
    /// actually came from. A card scheduled in two places would come due
    /// twice on schedules that drift apart, which is the whole point of
    /// routing rather than giving the deck a database of its own.
    #[test]
    fn test_custom_deck_routes_reviews_to_each_home_collection() -> Fallible<()> {
        use crate::cmd::drill::post::Action;
        use crate::cmd::serve::decks::ResolvedCustomDeck;
        use crate::cmd::serve::decks::slug_for_deck;

        // Two collections in one card tree: the deck draws on both, and
        // `deck_sources` finds them by slug through discovery.
        let dir = tempfile::tempdir()?;
        let data_dir = dir.path().to_path_buf();
        card_collection(
            &data_dir,
            None,
            "alpha",
            "Q: alpha question?\nA: alpha answer\n",
        )?;
        card_collection(
            &data_dir,
            None,
            "beta",
            "Q: beta question?\nA: beta answer\n",
        )?;
        let state = crate::cmd::serve::state::test_support::state_with_data_dir(data_dir.clone());
        let alpha = super::find_collection(&state, "alpha", None)
            .ok_or_else(|| ErrorReport::new("alpha was not discovered"))?;
        let beta = super::find_collection(&state, "beta", None)
            .ok_or_else(|| ErrorReport::new("beta was not discovered"))?;

        let deck = ResolvedCustomDeck {
            name: "Mixed".to_string(),
            slug: slug_for_deck("Mixed", None),
            owner: None,
            members: vec![
                crate::cmd::serve::config::DeckMember::parse("alpha/Deck")
                    .ok_or_else(|| ErrorReport::new("member"))?,
                crate::cmd::serve::config::DeckMember::parse("beta/Deck")
                    .ok_or_else(|| ErrorReport::new("member"))?,
            ],
        };
        state.custom_decks.lock().push(deck.clone());

        // The deck resolves to both collections.
        let sources = deck_sources(&state, &deck, None);
        assert_eq!(sources.len(), 2, "both collections must contribute");

        let session = create_session_from_sources(&state, sources, None)?
            .ok_or_else(|| ErrorReport::new("expected due cards from both collections"))?;
        let mut session = session;
        assert_eq!(
            session.mutable.cards.len(),
            2,
            "one new card from each collection"
        );

        // Grade every card in the session.
        while !session.mutable.cards.is_empty() {
            let hash = session.mutable.cards[0].hash();
            session.mutable.reveal = true;
            crate::cmd::drill::post::handle_action(
                &mut session.mutable,
                Action::Good,
                Some(hash),
                None,
            )?;
        }

        // Each collection's own database recorded exactly its own review.
        let alpha_db = open_collection_db(&state, &alpha)?;
        let beta_db = open_collection_db(&state, &beta)?;
        let alpha_reviews: usize = alpha_db
            .get_all_sessions()?
            .iter()
            .map(|s| {
                alpha_db
                    .get_reviews_for_session(s.session_id)
                    .map(|r| r.len())
            })
            .collect::<Fallible<Vec<usize>>>()?
            .iter()
            .sum();
        let beta_reviews: usize = beta_db
            .get_all_sessions()?
            .iter()
            .map(|s| {
                beta_db
                    .get_reviews_for_session(s.session_id)
                    .map(|r| r.len())
            })
            .collect::<Fallible<Vec<usize>>>()?
            .iter()
            .sum();
        assert_eq!(alpha_reviews, 1, "Alpha's card must be scheduled in Alpha");
        assert_eq!(beta_reviews, 1, "Beta's card must be scheduled in Beta");
        Ok(())
    }

    /// A deck's slug names no collection, so `/collection/{deck}/file/...`
    /// resolves to nothing and every image in the session 404s. The card's
    /// home collection is recorded on the session for exactly that reason —
    /// it used to be recorded only when a deck had more than one member, so
    /// a one-member deck inherited the deck's own unusable prefix.
    #[test]
    fn a_deck_session_serves_media_from_the_home_collection() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let data_dir = dir.path().to_path_buf();
        card_collection(
            &data_dir,
            None,
            "alpha",
            "Q: alpha question?\nA: alpha answer\n",
        )?;
        let state = crate::cmd::serve::state::test_support::state_with_data_dir(data_dir);
        let alpha = super::find_collection(&state, "alpha", None)
            .ok_or_else(|| ErrorReport::new("alpha was not discovered"))?;

        let session = create_session_from_sources(
            &state,
            vec![SessionSourceSpec {
                collection: alpha,
                decks: Vec::new(),
            }],
            None,
        )?
        .ok_or_else(|| ErrorReport::new("expected the new card to be due"))?;

        let hash = session.mutable.cards[0].hash();
        let source = session
            .mutable
            .dbs
            .for_card(hash)
            .source
            .as_ref()
            .ok_or_else(|| ErrorReport::new("the session recorded no source collection"))?;
        assert_eq!(source.file_url_prefix, "/collection/alpha/file");
        Ok(())
    }

    /// Cards are content addressed, so the same fact in two collections has
    /// one hash. A deck spanning both must still start: the session cache
    /// rejects a duplicate hash outright, so the second copy is skipped
    /// rather than failing the whole deck.
    #[test]
    fn test_custom_deck_tolerates_the_same_card_in_two_collections() -> Fallible<()> {
        let one_dir = tempfile::tempdir()?;
        let two_dir = tempfile::tempdir()?;
        // Byte-identical card text in both collections.
        let card = "Q: shared question?\nA: shared answer\n";
        std::fs::write(one_dir.path().join("Shared.md"), card)?;
        std::fs::write(two_dir.path().join("Shared.md"), card)?;

        let one = ResolvedCollection {
            name: "One".to_string(),
            slug: "one".to_string(),
            coll_dir: one_dir.path().to_path_buf(),
            db_path: one_dir.path().join("one.db"),
            collection_id: CollectionId::new("one")?,
            owner: None,
            overrides: Default::default(),
        };
        let two = ResolvedCollection {
            name: "Two".to_string(),
            slug: "two".to_string(),
            coll_dir: two_dir.path().to_path_buf(),
            db_path: two_dir.path().join("two.db"),
            collection_id: CollectionId::new("two")?,
            owner: None,
            overrides: Default::default(),
        };
        let data_dir = tempfile::tempdir()?;
        let state = crate::cmd::serve::state::test_support::state_with_data_dir(
            data_dir.path().to_path_buf(),
        );

        let sources = vec![
            SessionSourceSpec {
                collection: one,
                decks: vec!["Shared".to_string()],
            },
            SessionSourceSpec {
                collection: two,
                decks: vec!["Shared".to_string()],
            },
        ];
        let session = create_session_from_sources(&state, sources, None)?
            .ok_or_else(|| ErrorReport::new("expected the shared card to be due"))?;
        assert_eq!(
            session.mutable.cards.len(),
            1,
            "the shared card must appear once, not twice"
        );
        Ok(())
    }

    /// Serving a page must not write into the user's card folder: read
    /// paths open the tree without creating it and skip folders that have
    /// no id yet. Otherwise every collection page, stats page and media
    /// file would create directories — and fail outright on a read-only
    /// data directory.
    #[test]
    fn find_collection_does_not_write_into_the_local_tree() -> Fallible<()> {
        use crate::cmd::serve::cards::collection_id;
        use crate::cmd::serve::handlers::find_collection;
        use crate::cmd::serve::state::test_support::state_with_data_dir;

        let dir = tempfile::tempdir()?;
        let data_dir = dir.path().to_path_buf();
        let state = state_with_data_dir(data_dir.clone());

        assert!(find_collection(&state, "Spanish", None).is_none());
        assert!(
            !data_dir.join("cards").exists(),
            "a lookup created the local tree"
        );

        // A folder that already has an id is still found.
        let folder = data_dir.join("cards").join("default").join("Spanish");
        std::fs::create_dir_all(&folder)?;
        collection_id(&folder)?;
        assert!(find_collection(&state, "Spanish", None).is_some());
        Ok(())
    }
}
