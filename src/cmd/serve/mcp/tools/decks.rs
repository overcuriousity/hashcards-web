//! Creating, writing, moving and deleting decks.
//!
//! A deck is one Markdown file inside a collection. `move_decks` is the
//! tool this project waited for the per-user database to make possible: a
//! deck's cards are rows keyed by `(collection_id, card_hash)`, so moving
//! them between collections is an update rather than a transfer between two
//! database files.

use rmcp::ErrorData;
use rmcp::RoleServer;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::schemars;
use rmcp::schemars::JsonSchema;
use rmcp::service::RequestContext;
use rmcp::tool;
use rmcp::tool_router;
use serde::Deserialize;

use crate::cmd::run_blocking;
use crate::cmd::serve::auth::CurrentUser;
use crate::cmd::serve::cards::user_db_path;
use crate::cmd::serve::edit::file_mtime_ms;
use crate::cmd::serve::files::create_entry;
use crate::cmd::serve::files::delete_entry;
use crate::cmd::serve::files::save_file;
use crate::cmd::serve::files::user_root;
use crate::cmd::serve::mcp::server::HashcardsMcp;
use crate::cmd::serve::mcp::tools::read::collection_of;
use crate::cmd::serve::mcp::tools::read::to_mcp;
use crate::cmd::serve::state::AppState;
use crate::error::ErrorReport;
use crate::error::Fallible;
use crate::error::fail;
use crate::parser::parse_deck;
use crate::user_db::UserDatabase;

pub(super) fn create_deck_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
    deck: &str,
) -> Fallible<String> {
    collection_of(state, user, slug)?;
    // `create_entry` appends `.md` itself and refuses a name that would
    // escape the tree, so a deck name from a model is checked exactly as
    // one typed into the file manager is.
    create_entry(state, user, slug, deck, false)
}

pub(super) fn write_deck_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
    deck: &str,
    content: &str,
) -> Fallible<String> {
    collection_of(state, user, slug)?;
    let root = user_root(state, user)?;
    let entry = root.resolve_entry(&format!("{slug}/{deck}"))?;
    if !entry.path.is_file() {
        return fail(format!(
            "There is no deck called `{deck}` in `{slug}`. Create it first with create_deck."
        ));
    }
    // Read here rather than carried by the model: `save_file` re-checks it
    // just before the rename, which is what closes the window.
    let mtime = file_mtime_ms(&entry.path)?;
    save_file(state, user, &entry.rel, content, mtime)
}

pub(super) fn delete_deck_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
    deck: &str,
) -> Fallible<String> {
    collection_of(state, user, slug)?;
    delete_entry(state, user, &format!("{slug}/{deck}"))
}

/// Move a deck into another collection, review history and all.
///
/// `rename_entry` cannot do this: it takes a bare name and keeps the
/// parent, so it renames within a collection and never across. The move
/// itself is a rename on disk plus one update in the database -- the
/// schema's `on update cascade` carries each card's reviews and bookmark
/// with it.
///
/// The cards are read from the file *before* it moves, because after the
/// move the source collection no longer parses them.
pub(super) fn move_decks_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    from_slug: &str,
    deck: &str,
    to_slug: &str,
) -> Fallible<String> {
    let from = collection_of(state, user, from_slug)?;
    let to = collection_of(state, user, to_slug)?;
    if from.collection_id == to.collection_id {
        return fail(format!("`{deck}` is already in `{to_slug}`."));
    }

    let root = user_root(state, user)?;
    let source = root.resolve_entry(&format!("{from_slug}/{deck}"))?;
    if !source.path.is_file() {
        return fail(format!(
            "There is no deck called `{deck}` in `{from_slug}`."
        ));
    }
    let target = root.resolve_entry(&format!("{to_slug}/{deck}"))?;
    if target.path.exists() {
        return fail(format!(
            "`{to_slug}` already has a deck called `{deck}`. Rename one of them first."
        ));
    }

    // A live session on either collection is reading cards and writing
    // grades against them; moving the file out from under one strands
    // those grades. The same guard the file manager applies.
    refuse_if_drilling_either(state, from_slug, to_slug)?;

    // The whole collection, filtered to this file: a deck may sit in a
    // subfolder, so parsing only its parent would miss the collection's
    // frontmatter and, for a nested deck, find the wrong set of cards.
    // Card file paths are canonical, so the comparison is too.
    let canonical = source.path.canonicalize()?;
    let hashes: Vec<_> = parse_deck(&from.coll_dir)?
        .cards
        .into_iter()
        .filter(|c| c.file_path() == &canonical)
        .map(|c| c.hash())
        .collect();

    if let Some(parent) = target.path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(&source.path, &target.path)?;

    // After the bytes: if the rename fails the schedules must stay where
    // the cards still are.
    let db_dir = match &state.config.data_dir {
        Some(d) => d.join("db"),
        None => return fail("No data directory is configured."),
    };
    let path = user_db_path(&root, &db_dir)?;
    if path.is_file() {
        UserDatabase::open(&path)?.move_cards(&from.collection_id, &to.collection_id, &hashes)?;
    }
    Ok(format!("Moved `{deck}` from `{from_slug}` to `{to_slug}`."))
}

/// Refuse the move while a drill session is running on either side.
fn refuse_if_drilling_either(state: &AppState, from_slug: &str, to_slug: &str) -> Fallible<()> {
    let sessions = state.sessions.lock();
    for key in sessions.keys() {
        if key.slug() == from_slug || key.slug() == to_slug {
            return Err(ErrorReport::new(format!(
                "`{}` is being drilled right now, so its decks cannot be moved. Finish or end \
                 that session first.",
                key.slug()
            )));
        }
    }
    Ok(())
}

// ── Tools ────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct DeckArgs {
    /// The collection's slug, as returned by list_collections.
    pub collection: String,
    /// The deck file, relative to the collection folder, e.g. `verbs.md`.
    pub deck: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct WriteDeckArgs {
    /// The collection's slug, as returned by list_collections.
    pub collection: String,
    /// The deck file, relative to the collection folder, e.g. `verbs.md`.
    pub deck: String,
    /// The whole file. Cards are separated by a line containing only `---`;
    /// each is `Q:` then `A:`, or `C:` with `[cloze]` deletions. The file
    /// may open with TOML frontmatter between `---` lines, where `name`
    /// overrides the deck's name.
    pub content: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct MoveDeckArgs {
    /// The collection the deck is in now.
    pub from_collection: String,
    /// The deck file, relative to that collection's folder.
    pub deck: String,
    /// The collection to move it to.
    pub to_collection: String,
}

#[tool_router(router = deck_router, vis = "pub(crate)")]
impl HashcardsMcp {
    #[tool(
        description = "Create an empty deck -- a Markdown card file -- inside a collection. \
                       `.md` is appended if you leave it off."
    )]
    async fn create_deck(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<DeckArgs>,
    ) -> Result<String, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            create_deck_for(
                &state,
                caller.current_user().as_ref(),
                &args.collection,
                &args.deck,
            )
        })
        .await
        .map_err(to_mcp)
    }

    #[tool(
        description = "Replace a deck's entire contents. The text must parse as cards or the \
                       write is refused and the file is left exactly as it was. Cards are \
                       separated by a line containing only `---`; each is `Q:` then `A:`, or \
                       `C:` with `[cloze]` deletions. Cards whose text is unchanged keep their \
                       review history."
    )]
    async fn write_deck(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<WriteDeckArgs>,
    ) -> Result<String, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            write_deck_for(
                &state,
                caller.current_user().as_ref(),
                &args.collection,
                &args.deck,
                &args.content,
            )
        })
        .await
        .map_err(to_mcp)
    }

    #[tool(
        description = "Move a deck from one collection to another. Its cards keep their review \
                       history. A card the destination collection already has keeps the \
                       schedule it has there."
    )]
    async fn move_decks(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<MoveDeckArgs>,
    ) -> Result<String, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            move_decks_for(
                &state,
                caller.current_user().as_ref(),
                &args.from_collection,
                &args.deck,
                &args.to_collection,
            )
        })
        .await
        .map_err(to_mcp)
    }

    #[tool(
        description = "Delete a deck. It goes to the user's trash and can be restored with \
                       restore_from_trash; only the user can empty the trash, from the web \
                       interface."
    )]
    async fn delete_deck(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<DeckArgs>,
    ) -> Result<String, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            delete_deck_for(
                &state,
                caller.current_user().as_ref(),
                &args.collection,
                &args.deck,
            )
        })
        .await
        .map_err(to_mcp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::serve::cards::CardRoot;
    use crate::cmd::serve::cards::collection_id;
    use crate::cmd::serve::mcp::tools::read::list_cards_for;
    use crate::cmd::serve::mcp::tools::read::read_deck_for;
    use crate::cmd::serve::mcp::tools::tests::mcp_fixture;
    use crate::cmd::serve::mcp::tools::tests::other_users_collection;
    use crate::cmd::serve::trash::list_trash;
    use crate::types::card_hash::CardHash;
    use crate::types::timestamp::Timestamp;

    /// A second collection of the caller's own, to move decks into.
    fn second_collection(dir: &tempfile::TempDir) -> Fallible<()> {
        let root = CardRoot::for_user(dir.path(), None)?;
        std::fs::create_dir_all(root.path().join("German"))?;
        collection_id(&root.path().join("German"))?;
        Ok(())
    }

    #[test]
    fn a_created_deck_is_empty_and_readable() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        create_deck_for(&mcp.state, None, "Spanish", "nouns.md")?;
        // The file manager seeds a new card file with a template, so what
        // matters is that it exists and parses, not that it is empty.
        read_deck_for(&mcp.state, None, "Spanish", "nouns.md")?;
        Ok(())
    }

    #[test]
    fn a_written_deck_replaces_the_file() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        write_deck_for(
            &mcp.state,
            None,
            "Spanish",
            "verbs.md",
            "Q: beber\nA: to drink\n",
        )?;
        let cards = list_cards_for(&mcp.state, None, "Spanish", None, false, None, 50)?;
        assert_eq!(cards.len(), 1);
        assert!(cards[0].front.contains("beber"), "{}", cards[0].front);
        Ok(())
    }

    /// A buffer that does not parse never stays on disk.
    #[test]
    fn a_deck_that_does_not_parse_is_refused_and_the_file_is_unchanged() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let before = read_deck_for(&mcp.state, None, "Spanish", "verbs.md")?;
        assert!(write_deck_for(&mcp.state, None, "Spanish", "verbs.md", "A: dangling\n").is_err());
        assert_eq!(
            read_deck_for(&mcp.state, None, "Spanish", "verbs.md")?,
            before
        );
        Ok(())
    }

    #[test]
    fn a_deleted_deck_goes_to_the_trash() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        delete_deck_for(&mcp.state, None, "Spanish", "verbs.md")?;
        assert!(read_deck_for(&mcp.state, None, "Spanish", "verbs.md").is_err());
        let trashed = list_trash(dir.path(), "default")?;
        assert_eq!(trashed.len(), 1);
        assert_eq!(trashed[0].original_path, "Spanish/verbs.md");
        // The collection itself is untouched.
        assert!(
            CardRoot::for_user(dir.path(), None)?
                .path()
                .join("Spanish")
                .is_dir()
        );
        Ok(())
    }

    /// The point of the per-user database, and the reason this project
    /// waited for it: moving a deck between collections is a row update,
    /// not a transfer between two database files.
    #[test]
    fn a_moved_deck_takes_its_cards_and_their_history() -> Fallible<()> {
        use crate::cmd::serve::cards::user_db_path;
        use crate::user_db::UserDatabase;

        let (dir, mcp) = mcp_fixture()?;
        second_collection(&dir)?;
        let root = CardRoot::for_user(dir.path(), None)?;
        let spanish = collection_id(&root.path().join("Spanish"))?;
        let german = collection_id(&root.path().join("German"))?;

        let hex = list_cards_for(&mcp.state, None, "Spanish", None, false, Some("hablar"), 50)?[0]
            .hash
            .clone();
        let hash = CardHash::from_hex(&hex)?;
        let db = UserDatabase::open(&user_db_path(&root, &dir.path().join("db"))?)?;
        db.collection(spanish.clone())
            .insert_card(hash, Timestamp::now())?;

        move_decks_for(&mcp.state, None, "Spanish", "verbs.md", "German")?;

        assert_eq!(
            list_cards_for(&mcp.state, None, "German", None, false, None, 50)?.len(),
            2
        );
        assert!(list_cards_for(&mcp.state, None, "Spanish", None, false, None, 50)?.is_empty());
        assert!(
            db.collection(german).card_hashes()?.contains(&hash),
            "the review history did not follow the deck"
        );
        assert!(db.collection(spanish).card_hashes()?.is_empty());
        Ok(())
    }

    #[test]
    fn moving_onto_an_existing_deck_name_is_refused() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        second_collection(&dir)?;
        create_deck_for(&mcp.state, None, "German", "verbs.md")?;
        let err = move_decks_for(&mcp.state, None, "Spanish", "verbs.md", "German").unwrap_err();
        assert!(err.message().contains("already has"), "{}", err.message());
        // And nothing moved.
        assert_eq!(
            list_cards_for(&mcp.state, None, "Spanish", None, false, None, 50)?.len(),
            2
        );
        Ok(())
    }

    #[test]
    fn a_deck_path_that_escapes_the_collection_is_refused() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        assert!(create_deck_for(&mcp.state, None, "Spanish", "../escaped.md").is_err());
        assert!(
            write_deck_for(&mcp.state, None, "Spanish", "../escaped.md", "Q: a\nA: b\n").is_err()
        );
        assert!(delete_deck_for(&mcp.state, None, "Spanish", "../escaped.md").is_err());
        Ok(())
    }

    #[test]
    fn another_users_collection_is_refused() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let theirs = other_users_collection(&dir)?;
        assert!(create_deck_for(&mcp.state, None, &theirs, "extra.md").is_err());
        assert!(write_deck_for(&mcp.state, None, &theirs, "nouns.md", "Q: a\nA: b\n").is_err());
        assert!(delete_deck_for(&mcp.state, None, &theirs, "nouns.md").is_err());
        assert!(move_decks_for(&mcp.state, None, &theirs, "nouns.md", "Spanish").is_err());
        Ok(())
    }
}
