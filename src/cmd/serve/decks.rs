//! User-assembled decks: a named selection of decks drawn from any of the
//! owner's collections, drilled as a single session.
//!
//! A custom deck owns no cards and no database. It is a saved selection, and
//! drilling it opens each contributing collection's own database so every
//! review lands where the card actually lives (see `SessionDbs`). A card
//! included in three custom decks still has exactly one schedule.

use std::collections::HashMap;
use std::path::Path;

use crate::cmd::serve::config::CustomDeckEntry;
use crate::cmd::serve::config::DeckMember;
use crate::cmd::serve::config::ResolvedCollection;
use crate::cmd::serve::config::slugify;
use axum::Form;
use axum::extract::Query;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Html;
use axum::response::Redirect;
use maud::Markup;
use maud::html;

use crate::cmd::drill::template::page_template;
use crate::cmd::serve::auth::CurrentUser;
use crate::cmd::serve::files::collections_for_user;
use crate::cmd::serve::handlers::current_user_for;
use crate::cmd::serve::reviewdb::open_collection_db;
use crate::cmd::serve::state::AppState;
use crate::collection::Collection;
use crate::error::Fallible;
use crate::error::fail;
use crate::flash::Flash;

/// A custom deck with its URL slug resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedCustomDeck {
    pub name: String,
    pub slug: String,
    pub owner: Option<String>,
    pub members: Vec<DeckMember>,
}

/// Build a custom deck's URL slug.
///
/// Keyed by (owner, name) so two users may each have a deck called
/// "Exam revision" without colliding, and prefixed `deck-` so a custom deck
/// can never be confused with a collection.
pub fn slug_for_deck(name: &str, owner: Option<&str>) -> String {
    let stem = slugify(name);
    let stem = if stem.is_empty() {
        "deck"
    } else {
        stem.as_str()
    };
    let keyed = match owner {
        Some(owner) => format!("{owner}\n{name}"),
        None => name.to_string(),
    };
    format!(
        "deck-{}-{}",
        stem,
        &blake3::hash(keyed.as_bytes()).to_hex()[..8]
    )
}

/// Resolve the configured entries, dropping any whose members are malformed
/// (config load already rejects those, so this is belt-and-braces).
pub fn resolve_custom_decks(entries: &[CustomDeckEntry]) -> Vec<ResolvedCustomDeck> {
    entries
        .iter()
        .map(|e| ResolvedCustomDeck {
            slug: slug_for_deck(&e.name, e.owner.as_deref()),
            name: e.name.clone(),
            owner: e.owner.clone(),
            members: e
                .members
                .iter()
                .filter_map(|m| DeckMember::parse(m))
                .collect(),
        })
        .collect()
}

/// Look up a custom deck by slug, scoped to `owner`. A deck belonging to
/// someone else is indistinguishable from one that does not exist.
pub fn find_custom_deck(
    decks: &[ResolvedCustomDeck],
    slug: &str,
    owner: Option<&str>,
) -> Option<ResolvedCustomDeck> {
    decks
        .iter()
        .find(|d| d.slug == slug && d.owner.as_deref() == owner)
        .cloned()
}

/// Reject a deck whose slug collides with a collection's, which would make
/// `/collection/{slug}` ambiguous.
pub fn check_deck_slug_collisions(
    decks: &[ResolvedCustomDeck],
    collections: &[ResolvedCollection],
) -> Fallible<()> {
    let mut seen: HashMap<&str, String> = HashMap::new();
    for c in collections {
        seen.insert(c.slug.as_str(), format!("collection '{}'", c.name));
    }
    for d in decks {
        if let Some(first) = seen.get(d.slug.as_str()) {
            return fail(format!(
                "configuration error: {first} and deck '{}' both map to the URL slug '{}'. \
                 Rename one of them.",
                d.name, d.slug
            ));
        }
        seen.insert(d.slug.as_str(), format!("deck '{}'", d.name));
    }
    Ok(())
}

/// Write the `[[deck]]` entries back to the config file, preserving
/// everything else in it.
///
/// `owner` is written whenever it is set: dropping it would leave a config
/// that `[oidc]` validation rejects at the next startup.
pub fn persist_custom_decks(config_path: &Path, entries: &[CustomDeckEntry]) -> Fallible<()> {
    let content = std::fs::read_to_string(config_path)?;
    let mut doc: toml::Value = toml::from_str(&content)?;
    let table = doc
        .as_table_mut()
        .ok_or_else(|| crate::error::ErrorReport::new("Config is not a TOML table"))?;

    if entries.is_empty() {
        table.remove("deck");
    } else {
        let array: Vec<toml::Value> = entries
            .iter()
            .map(|e| {
                let mut t = toml::map::Map::new();
                t.insert("name".to_string(), toml::Value::String(e.name.clone()));
                t.insert(
                    "members".to_string(),
                    toml::Value::Array(
                        e.members
                            .iter()
                            .map(|m| toml::Value::String(m.clone()))
                            .collect(),
                    ),
                );
                if let Some(owner) = &e.owner {
                    t.insert("owner".to_string(), toml::Value::String(owner.clone()));
                }
                toml::Value::Table(t)
            })
            .collect();
        table.insert("deck".to_string(), toml::Value::Array(array));
    }

    let serialized = toml::to_string_pretty(&doc)?;
    // Atomic write, matching `persist_source_entries`: a crash mid-write
    // must not truncate the user's config.
    static WRITE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = WRITE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = config_path.parent().unwrap_or(Path::new("."));
    let tmp_path = dir.join(format!(".hashcards-decks-{}-{}.tmp", std::process::id(), n));
    std::fs::write(&tmp_path, serialized)?;
    #[cfg(windows)]
    if std::fs::metadata(config_path).is_ok() {
        if let Err(e) = std::fs::remove_file(config_path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e.into());
        }
    }
    if let Err(e) = std::fs::rename(&tmp_path, config_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    Ok(())
}

/// Every deck the caller could add to a custom deck, grouped by collection.
pub(super) struct DeckChoices {
    pub collection_name: String,
    pub collection_slug: String,
    pub deck_names: Vec<String>,
}

/// Read the deck names out of each of the caller's collections.
///
/// A collection that fails to load is skipped rather than failing the page:
/// one broken markdown file should not make every other deck unpickable.
pub(super) fn deck_choices(
    state: &AppState,
    collections: &[ResolvedCollection],
) -> Vec<DeckChoices> {
    let mut out = Vec::new();
    for rc in collections {
        let collection = match open_collection_db(state, rc)
            .and_then(|db| Collection::open(rc.coll_dir.clone(), db))
        {
            Ok(c) => c,
            Err(e) => {
                log::warn!("skipping collection '{}' while listing decks: {e}", rc.name);
                continue;
            }
        };
        let mut deck_names: Vec<String> = collection
            .cards
            .iter()
            .map(|c| c.deck_name().to_string())
            .collect();
        deck_names.sort();
        deck_names.dedup();
        if deck_names.is_empty() {
            continue;
        }
        out.push(DeckChoices {
            collection_name: rc.name.clone(),
            collection_slug: rc.slug.clone(),
            deck_names,
        });
    }
    out
}

/// The `[[deck]]` entries as they should be written to the config file.
fn entries_from(decks: &[ResolvedCustomDeck]) -> Vec<CustomDeckEntry> {
    decks
        .iter()
        .map(|d| CustomDeckEntry {
            name: d.name.clone(),
            owner: d.owner.clone(),
            members: d.members.iter().map(|m| m.encode()).collect(),
        })
        .collect()
}

/// Render the deck management page.
pub(super) fn render_decks_page(
    decks: &[ResolvedCustomDeck],
    choices: &[DeckChoices],
    config_available: bool,
    flash: Option<Flash>,
) -> Markup {
    page_template(html! {
        @if let Some(f) = &flash { (f.render()) }
        div.landing {
            h1 { "Decks" }
            p { a.back-link href="/" { "\u{2190} Back to collections" } }
            p.empty {
                "A deck is a saved selection of topics from any of your collections, drilled \
                 together. Reviews still count towards each card's own collection, so a card \
                 in several decks keeps one schedule."
            }

            @if !config_available {
                div.notice {
                    p { "Decks cannot be saved without a configured data directory." }
                    p { "Start hashcards-web with " code { "--config hashcards.toml" } "." }
                }
            } @else {
                h2 { "New deck" }
                @if choices.is_empty() {
                    p.empty { "No collections with topics to choose from yet." }
                } @else {
                    form action="/decks/add" method="post" {
                        div.add-source-row {
                            input .add-source-url type="text" name="name"
                                placeholder="Deck name, e.g. Exam revision" required;
                        }
                        @for choice in choices {
                            fieldset {
                                legend { (choice.collection_name) }
                                @for deck_name in &choice.deck_names {
                                    label {
                                        input type="checkbox" name="members"
                                            value=(format!("{}/{}", choice.collection_slug, deck_name));
                                        " " (deck_name)
                                    }
                                }
                            }
                        }
                        input .btn.btn-primary type="submit" value="Create deck";
                    }
                }

                h2 { "Your decks" }
                @if decks.is_empty() {
                    p.empty { "No decks yet." }
                } @else {
                    table.collection-table {
                        thead { tr { th { "Deck" } th { "Members" } th { "" } } }
                        tbody {
                            @for deck in decks {
                                tr {
                                    td { a href=(format!("/collection/{}", deck.slug)) { (deck.name) } }
                                    td {
                                        (deck.members.iter().map(|m| m.encode())
                                            .collect::<Vec<_>>().join(", "))
                                    }
                                    td {
                                        form action="/decks/delete" method="post" {
                                            input type="hidden" name="name" value=(deck.name);
                                            input type="submit" value="Delete" .sync-button
                                                onclick="return confirm('Delete this deck? The cards and their review history are not touched.')";
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

/// Every collection `owner` can put in a custom deck: configured ones,
/// their own card folders.
///
/// One list for the picker and for the ownership check, so a collection can
/// never be offered on `/decks` and then refused when it is chosen.
///
/// Blocking: local collections are discovered by reading the tree.
fn owned_collections(state: &AppState, owner: Option<&str>) -> Vec<ResolvedCollection> {
    collections_for_user(state, current_user_for(owner).as_ref())
}

// ---- HTTP handlers ----

/// Render the deck management page, scoped to the caller.
pub async fn decks_manage_handler(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    current_user: Option<CurrentUser>,
) -> (StatusCode, Html<String>) {
    let flash = Flash::from_query(&query);
    let owner = current_user.map(|u| u.email);
    let mine: Vec<ResolvedCustomDeck> = state
        .custom_decks
        .lock()
        .iter()
        .filter(|d| d.owner.as_deref() == owner.as_deref())
        .cloned()
        .collect();
    let config_available = state.config.data_dir.is_some();
    // Reading every collection off disk is blocking work, and so is
    // discovering the local ones; keep both off the async executor
    // (BUG-44).
    let state2 = state.clone();
    let owner2 = owner.clone();
    let choices = match tokio::task::spawn_blocking(move || {
        deck_choices(&state2, &owned_collections(&state2, owner2.as_deref()))
    })
    .await
    {
        Ok(choices) => choices,
        Err(e) => {
            log::error!("failed to list decks: {e}");
            Vec::new()
        }
    };
    let html = render_decks_page(&mine, &choices, config_available, flash);
    (StatusCode::OK, Html(html.into_string()))
}

pub struct AddDeckForm {
    pub name: String,
    pub members: Vec<String>,
}

/// Custom `Deserialize` for `AddDeckForm`, for the same reason
/// `StartDrillForm` has one: `serde_urlencoded` presents the repeated
/// checkbox key (`members=a/b&members=c/d`) as separate map entries rather
/// than a sequence, and the derived impl rejects that as a duplicate field.
impl<'de> serde::Deserialize<'de> for AddDeckForm {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::MapAccess;
        use serde::de::Visitor;

        struct FormVisitor;

        impl<'de> Visitor<'de> for FormVisitor {
            type Value = AddDeckForm;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a form with a name and repeated members fields")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut name = String::new();
                let mut members = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "name" => name = map.next_value::<String>()?,
                        "members" => members.push(map.next_value::<String>()?),
                        _ => {
                            let _ = map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(AddDeckForm { name, members })
            }
        }

        deserializer.deserialize_map(FormVisitor)
    }
}

/// Create a custom deck owned by the caller.
pub async fn deck_add_handler(
    State(state): State<AppState>,
    current_user: Option<CurrentUser>,
    Form(form): Form<AddDeckForm>,
) -> Redirect {
    let owner = current_user.map(|u| u.email);
    let name = form.name.trim().to_string();
    if name.is_empty() {
        return Flash::error("Give the deck a name.").redirect("/decks");
    }
    if form.members.is_empty() {
        return Flash::error("Select at least one deck to include.").redirect("/decks");
    }

    // Every member must name a collection the caller actually owns, or a
    // deck could be used to read another user's cards. Listed once, off the
    // executor: discovering local collections reads the user's tree.
    let state2 = state.clone();
    let owner2 = owner.clone();
    let owned =
        match tokio::task::spawn_blocking(move || owned_collections(&state2, owner2.as_deref()))
            .await
        {
            Ok(owned) => owned,
            Err(e) => {
                log::error!("failed to list collections: {e}");
                return Flash::error("Decks could not be listed. Try again.").redirect("/decks");
            }
        };
    let mut members = Vec::new();
    for raw in &form.members {
        let Some(member) = DeckMember::parse(raw) else {
            return Flash::error(format!("Malformed deck selection: {raw}")).redirect("/decks");
        };
        if !owned.iter().any(|c| c.slug == member.collection_slug) {
            return Flash::error("That selection includes a collection you don't own.")
                .redirect("/decks");
        }
        members.push(member);
    }

    let Some(config_path) = state.config_path.lock().clone() else {
        return Flash::error(
            "Decks cannot be saved: no config file is in use. Start hashcards-web with --config.",
        )
        .redirect("/decks");
    };

    let slug = slug_for_deck(&name, owner.as_deref());
    let new_deck = ResolvedCustomDeck {
        name: name.clone(),
        slug: slug.clone(),
        owner: owner.clone(),
        members,
    };

    // Duplicate check, mutation and persist under one lock, so concurrent
    // adds cannot write a config missing each other's decks (BUG-39).
    let decks_arc = state.custom_decks.clone();
    // Checked against the caller's own collections, discovered a moment ago
    // above: a deck slug carries a `deck-` prefix and a hash, so a collision
    // means a folder deliberately named after a deck.
    let collections = owned;
    let persisted = tokio::task::spawn_blocking(move || {
        let mut guard = decks_arc.lock();
        if guard
            .iter()
            .any(|d| d.slug == new_deck.slug || (d.name == name && d.owner == new_deck.owner))
        {
            return fail(format!("You already have a deck called '{name}'."));
        }
        let mut updated = guard.clone();
        updated.push(new_deck);
        check_deck_slug_collisions(&updated, &collections)?;
        persist_custom_decks(&config_path, &entries_from(&updated))?;
        *guard = updated;
        Ok(())
    })
    .await;

    match persisted {
        Ok(Ok(())) => Flash::success("Deck created.").redirect("/decks"),
        Ok(Err(e)) => Flash::error(e.to_string()).redirect("/decks"),
        Err(e) => {
            log::error!("deck add task panicked: {e}");
            Flash::error("Failed to create the deck.").redirect("/decks")
        }
    }
}

#[derive(serde::Deserialize)]
pub struct DeleteDeckForm {
    pub name: String,
}

/// Delete one of the caller's own decks. The cards and their review history
/// live in their collections and are untouched.
pub async fn deck_delete_handler(
    State(state): State<AppState>,
    current_user: Option<CurrentUser>,
    Form(form): Form<DeleteDeckForm>,
) -> Redirect {
    let owner = current_user.map(|u| u.email);
    let Some(config_path) = state.config_path.lock().clone() else {
        return Flash::error("Decks cannot be saved: no config file is in use.").redirect("/decks");
    };
    let decks_arc = state.custom_decks.clone();
    let name = form.name.clone();
    let deleted = tokio::task::spawn_blocking(move || {
        let mut guard = decks_arc.lock();
        // Matched on (name, owner): deleting your own deck must never remove
        // another user's deck of the same name.
        let is_target =
            |d: &ResolvedCustomDeck| d.name == name && d.owner.as_deref() == owner.as_deref();
        if !guard.iter().any(is_target) {
            return fail(format!("No deck called '{name}'."));
        }
        let mut updated = guard.clone();
        updated.retain(|d| !is_target(d));
        persist_custom_decks(&config_path, &entries_from(&updated))?;
        *guard = updated;
        Ok(())
    })
    .await;

    match deleted {
        Ok(Ok(())) => Flash::success("Deck deleted. Its cards and review history are untouched.")
            .redirect("/decks"),
        Ok(Err(e)) => Flash::error(e.to_string()).redirect("/decks"),
        Err(e) => {
            log::error!("deck delete task panicked: {e}");
            Flash::error("Failed to delete the deck.").redirect("/decks")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::collection_id::CollectionId;

    fn entry(name: &str, owner: Option<&str>, members: &[&str]) -> CustomDeckEntry {
        CustomDeckEntry {
            name: name.to_string(),
            owner: owner.map(|o| o.to_string()),
            members: members.iter().map(|m| m.to_string()).collect(),
        }
    }

    /// A local card folder is a collection like any other: it must be
    /// offered in the deck picker, and the ownership check has to accept it
    /// — otherwise choosing one is refused with "a collection you don't
    /// own", for a folder the caller made themselves.
    #[test]
    fn local_collections_can_be_put_in_a_custom_deck() -> Fallible<()> {
        let dir = crate::helper::create_tmp_directory()?;
        let state = crate::cmd::serve::state::test_support::state_with_data_dir(dir.clone());
        let root = crate::cmd::serve::files::user_root(&state, None)?;
        std::fs::create_dir_all(root.path().join("Spanish"))?;
        std::fs::write(root.path().join("Spanish").join("verbs.md"), "Q: a\nA: b\n")?;

        let owned = owned_collections(&state, None);
        assert!(
            owned.iter().any(|c| c.slug == "Spanish"),
            "local collections: {:?}",
            owned.iter().map(|c| &c.slug).collect::<Vec<_>>()
        );
        Ok(())
    }

    #[test]
    fn test_member_round_trip() {
        let m = DeckMember::parse("japanese/Verbs").expect("well-formed");
        assert_eq!(m.collection_slug, "japanese");
        assert_eq!(m.deck_name, "Verbs");
        assert_eq!(m.encode(), "japanese/Verbs");
    }

    /// Deck names mirror the file tree and may contain `/`, so only the first
    /// separator splits.
    #[test]
    fn test_member_keeps_slashes_in_the_deck_name() {
        let m = DeckMember::parse("japanese/Grammar/Particles").expect("well-formed");
        assert_eq!(m.collection_slug, "japanese");
        assert_eq!(m.deck_name, "Grammar/Particles");
    }

    #[test]
    fn test_malformed_members_are_rejected() {
        assert!(DeckMember::parse("no-separator").is_none());
        assert!(DeckMember::parse("/leading").is_none());
        assert!(DeckMember::parse("trailing/").is_none());
    }

    /// Two users may each name a deck the same thing without colliding.
    #[test]
    fn test_slug_is_keyed_by_owner_and_name() {
        let alice = slug_for_deck("Exam revision", Some("alice@example.com"));
        let bob = slug_for_deck("Exam revision", Some("bob@example.com"));
        assert_ne!(alice, bob);
        // `slugify` preserves case, as it does for collection and note slugs.
        assert!(alice.starts_with("deck-Exam-revision-"), "{alice}");
    }

    #[test]
    fn test_find_is_scoped_to_owner() {
        let decks = resolve_custom_decks(&[entry(
            "Mine",
            Some("alice@example.com"),
            &["japanese/Verbs"],
        )]);
        let slug = decks[0].slug.clone();
        assert!(find_custom_deck(&decks, &slug, Some("alice@example.com")).is_some());
        assert!(find_custom_deck(&decks, &slug, Some("bob@example.com")).is_none());
        assert!(find_custom_deck(&decks, &slug, None).is_none());
    }

    /// `owner` must survive the config rewrite, or the next startup fails
    /// `[oidc]` validation and the deck's slug would change.
    #[test]
    fn test_persist_keeps_name_members_and_owner() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("hashcards.toml");
        std::fs::write(&config_path, "[server]\ndata_dir = \"/tmp\"\n")?;

        let entries = vec![entry(
            "Exam revision",
            Some("alice@example.com"),
            &["japanese/Verbs", "math/Algebra"],
        )];
        persist_custom_decks(&config_path, &entries)?;

        let content = std::fs::read_to_string(&config_path)?;
        let config: crate::cmd::serve::config::ServeConfig = toml::from_str(&content)?;
        assert_eq!(config.decks.len(), 1);
        assert_eq!(config.decks[0].name, "Exam revision");
        assert_eq!(config.decks[0].owner.as_deref(), Some("alice@example.com"));
        assert_eq!(
            config.decks[0].members,
            vec!["japanese/Verbs".to_string(), "math/Algebra".to_string()]
        );
        Ok(())
    }

    #[test]
    fn test_persist_empty_removes_the_section() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("hashcards.toml");
        std::fs::write(
            &config_path,
            "[server]\ndata_dir = \"/tmp\"\n\n[[deck]]\nname = \"Gone\"\nmembers = []\n",
        )?;
        persist_custom_decks(&config_path, &[])?;
        let content = std::fs::read_to_string(&config_path)?;
        assert!(!content.contains("[[deck]]"), "config: {content}");
        Ok(())
    }

    #[test]
    fn test_slug_collision_with_a_collection_is_rejected() {
        let decks = resolve_custom_decks(&[entry("Mine", None, &["a/b"])]);
        let collections = vec![ResolvedCollection {
            name: "Sneaky".to_string(),
            slug: decks[0].slug.clone(),
            coll_dir: std::path::PathBuf::from("/tmp/a"),
            db_path: std::path::PathBuf::from("/tmp/a.db"),
            collection_id: CollectionId::new("test-collection").expect("a non-empty id"),
            owner: None,
            overrides: Default::default(),
        }];
        assert!(check_deck_slug_collisions(&decks, &collections).is_err());
        assert!(check_deck_slug_collisions(&decks, &[]).is_ok());
    }
}
