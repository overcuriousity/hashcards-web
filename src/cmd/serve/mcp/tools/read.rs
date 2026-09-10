//! Reading collections, decks, cards and statistics.
//!
//! Every one of these goes through `existing_collections_for_user`, never
//! `collections_for_user`: the latter uses `IdPolicy::CreateMissing` and
//! will write a `.hashcards.toml` into a folder that has none. A tool
//! described as read-only must not create anything.

use rmcp::ErrorData;
use rmcp::RoleServer;
use rmcp::handler::server::wrapper::Json;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::service::RequestContext;
use rmcp::tool;
use rmcp::tool_router;
// The derive generates `schemars::…` paths, so the crate has to be in
// scope under that name. Taken from rmcp's re-export rather than added as
// a direct dependency, so there is one version of it and not two.
use rmcp::schemars;
use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

use crate::cmd::run_blocking;
use crate::cmd::serve::auth::CurrentUser;
use crate::cmd::serve::cards::CardRoot;
use crate::cmd::serve::config::ResolvedCollection;
use crate::cmd::serve::files::existing_collections_for_user;
use crate::cmd::serve::files::user_root_readonly;
use crate::cmd::serve::handlers::find_collection;
use crate::cmd::serve::mcp::server::HashcardsMcp;
use crate::cmd::serve::reviewdb::open_collection_db;
use crate::cmd::serve::state::AppState;
use crate::cmd::stats_page::gather_stats;
use crate::error::ErrorReport;
use crate::error::Fallible;
use crate::error::fail;
use crate::parser::parse_deck;
use crate::types::card::Card;
use crate::types::card::CardContent;
use crate::types::card_hash::CardHash;
use crate::types::timestamp::Timestamp;

/// Every tool answers a `Fallible`, and every failure reaches the model as
/// the message that would have reached a person.
///
/// That is not a shortcut: these messages are already written to be read by
/// whoever has to fix the problem, and here that reader is the one who can
/// fix it fastest.
pub(super) fn to_mcp(e: ErrorReport) -> ErrorData {
    ErrorData::invalid_params(e.message().to_string(), None)
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CollectionSummary {
    /// What every other tool calls this collection.
    pub slug: String,
    /// Its folder name, as the user sees it.
    pub name: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CardSummary {
    /// The card's content address. It changes when the card's text changes.
    pub hash: String,
    /// The deck the card is in.
    pub deck: String,
    /// The question, or the cloze text.
    pub front: String,
    /// "basic" or "cloze".
    pub kind: String,
    /// Whether the card is due today. A card that has never been reviewed
    /// is due.
    pub due: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ReviewSummary {
    pub reviewed_at: String,
    pub grade: String,
    pub due_date: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CardDetail {
    pub hash: String,
    pub deck: String,
    /// The card's Markdown source, exactly as it is in the file.
    pub source: String,
    pub front: String,
    /// The answer, or the text hidden by the cloze deletion.
    pub back: String,
    pub kind: String,
    pub due: bool,
    /// Surviving reviews, oldest first. A review the user undid is not
    /// here: they took it back.
    pub reviews: Vec<ReviewSummary>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DeckCounts {
    pub deck: String,
    pub due: usize,
    pub total: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CollectionStatistics {
    pub total_cards: usize,
    pub due_today: usize,
    /// Fraction of recent reviews graded better than Forgot, if there have
    /// been any.
    pub retention: Option<f64>,
    pub decks: Vec<DeckCounts>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct UserStatistics {
    pub collections: Vec<CollectionCounts>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CollectionCounts {
    pub slug: String,
    pub name: String,
    pub total_cards: usize,
    pub due_today: usize,
}

pub(super) fn list_collections_for(
    state: &AppState,
    user: Option<&CurrentUser>,
) -> Fallible<Vec<CollectionSummary>> {
    Ok(existing_collections_for_user(state, user)
        .into_iter()
        .map(|c| CollectionSummary {
            slug: c.slug,
            name: c.name,
        })
        .collect())
}

/// The collection `slug` names, refused by name when it is not there.
///
/// Scoped to the caller: another user's collection is not "forbidden", it
/// simply is not among theirs, which is also the honest answer.
pub(super) fn collection_of(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
) -> Fallible<ResolvedCollection> {
    let owner = user.map(|u| u.email.to_lowercase());
    find_collection(state, slug, owner.as_deref()).ok_or_else(|| {
        ErrorReport::new(format!(
            "There is no collection called `{slug}`. Use list_collections to see what there is."
        ))
    })
}

/// The collection's folder, as a path relative to the caller's card tree.
///
/// Not the slug. `slugify` maps every character that is not alphanumeric to
/// `-`, so a collection whose folder is `Exam revision` is addressed as
/// `Exam-revision` -- and there is no folder of that name. Every tool that
/// builds a filesystem path takes it from here, because a path built out of
/// the slug resolves to nothing for any collection whose name contains a
/// space or a piece of punctuation, and the tools then answer that its
/// decks do not exist.
///
/// Derived from `coll_dir` rather than from `name`, so the folder this
/// returns is by construction the one the collection was discovered in.
pub(super) fn collection_folder(root: &CardRoot, rc: &ResolvedCollection) -> Fallible<String> {
    let outside = || {
        ErrorReport::new(format!(
            "`{}` is not a folder inside your card directory, so it cannot be edited here.",
            rc.name
        ))
    };
    let relative = match rc.coll_dir.strip_prefix(root.path()) {
        Ok(rel) => rel.to_path_buf(),
        // Canonical on both sides, for a data directory reached through a
        // symbolic link.
        Err(_) => match (rc.coll_dir.canonicalize(), root.path().canonicalize()) {
            (Ok(coll), Ok(base)) => coll
                .strip_prefix(&base)
                .map_err(|_| outside())?
                .to_path_buf(),
            _ => return Err(outside()),
        },
    };
    // A collection is a top-level folder, so its path inside the tree is a
    // single component. Anything else is not one of ours.
    let folder = relative.to_str().ok_or_else(outside)?;
    if folder.is_empty() || folder.contains('/') || folder.contains('\\') {
        return Err(outside());
    }
    Ok(folder.to_string())
}

/// A deck file inside a collection, resolved as hard as a path from a
/// browser: `CardRoot` refuses `..` and symbolic links, and the result is
/// checked to be inside the collection it claimed to be in.
fn deck_path(
    state: &AppState,
    user: Option<&CurrentUser>,
    rc: &ResolvedCollection,
    slug: &str,
    deck: &str,
) -> Fallible<std::path::PathBuf> {
    let root = user_root_readonly(state, user)?;
    let entry = root.resolve_entry(&format!("{}/{deck}", collection_folder(&root, rc)?))?;
    let inside = match (entry.path.canonicalize(), rc.coll_dir.canonicalize()) {
        (Ok(p), Ok(c)) => p.starts_with(c),
        _ => false,
    };
    if !inside || !entry.path.is_file() {
        return fail(format!("There is no deck called `{deck}` in `{slug}`."));
    }
    Ok(entry.path)
}

pub(super) fn read_deck_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
    deck: &str,
) -> Fallible<String> {
    let rc = collection_of(state, user, slug)?;
    let path = deck_path(state, user, &rc, slug, deck)?;
    Ok(std::fs::read_to_string(&path)?)
}

/// The question (or cloze text) and the answer (or the deleted span), as
/// plain text.
fn front_and_back(card: &Card) -> (String, String) {
    match card.content() {
        CardContent::Basic { question, answer } => (question.clone(), answer.clone()),
        // Byte positions, not character positions -- the deletion is a byte
        // range into the text and slicing it any other way splits a
        // multi-byte character.
        CardContent::Cloze { text, start, end } => {
            let hidden = text.get(*start..=*end).unwrap_or_default().to_string();
            (text.clone(), hidden)
        }
    }
}

fn kind_of(card: &Card) -> String {
    match card.card_type() {
        crate::types::card::CardType::Basic => "basic".to_string(),
        crate::types::card::CardType::Cloze => "cloze".to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn list_cards_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
    deck: Option<&str>,
    due_only: bool,
    query: Option<&str>,
    limit: usize,
) -> Fallible<Vec<CardSummary>> {
    let rc = collection_of(state, user, slug)?;
    let cards = parse_deck(&rc.coll_dir)?.cards;
    let db = open_collection_db(state, &rc)?;
    let due = db.due_today(Timestamp::now().date())?;
    let seen = db.card_hashes()?;
    let needle = query.map(|q| q.to_lowercase());

    let mut out = Vec::new();
    for card in &cards {
        if let Some(deck) = deck {
            if card.deck_name().as_str() != deck {
                continue;
            }
        }
        // A card nothing has ever reviewed has no row, and is due: it has
        // never been seen.
        let is_due = due.contains(&card.hash()) || !seen.contains(&card.hash());
        if due_only && !is_due {
            continue;
        }
        let (front, back) = front_and_back(card);
        if let Some(needle) = &needle {
            let hit = front.to_lowercase().contains(needle) || back.to_lowercase().contains(needle);
            if !hit {
                continue;
            }
        }
        out.push(CardSummary {
            hash: card.hash().to_string(),
            deck: card.deck_name().to_string(),
            front,
            kind: kind_of(card),
            due: is_due,
        });
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

pub(super) fn get_card_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
    hash_hex: &str,
) -> Fallible<CardDetail> {
    let rc = collection_of(state, user, slug)?;
    let hash = CardHash::from_hex(hash_hex)?;
    let cards = parse_deck(&rc.coll_dir)?.cards;
    let card = cards.iter().find(|c| c.hash() == hash).ok_or_else(|| {
        ErrorReport::new(format!(
            "There is no card with hash `{hash_hex}` in `{slug}`. A card hash is a content \
             address, so it changes whenever the card's text changes -- if you read this card \
             earlier, it has been changed or moved since. List the cards again to find it."
        ))
    })?;

    let db = open_collection_db(state, &rc)?;
    let due = db.due_today(Timestamp::now().date())?;
    let seen = db.card_hashes()?;
    let reviews = db
        .reviews_for_card(hash)?
        .into_iter()
        .map(|r| ReviewSummary {
            reviewed_at: r.data.reviewed_at.to_string(),
            grade: format!("{:?}", r.data.grade).to_lowercase(),
            due_date: r.data.due_date.to_string(),
        })
        .collect();

    let source = std::fs::read_to_string(card.file_path())
        .ok()
        .and_then(|content| {
            crate::cmd::serve::edit::extract_card_block(&content, card.range()).ok()
        })
        .unwrap_or_default();
    let (front, back) = front_and_back(card);
    Ok(CardDetail {
        hash: hash_hex.to_string(),
        deck: card.deck_name().to_string(),
        source,
        front,
        back,
        kind: kind_of(card),
        due: due.contains(&hash) || !seen.contains(&hash),
        reviews,
    })
}

pub(super) fn collection_stats_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
) -> Fallible<CollectionStatistics> {
    let rc = collection_of(state, user, slug)?;
    let cards = parse_deck(&rc.coll_dir)?.cards;
    let db = open_collection_db(state, &rc)?;
    let stats = gather_stats(&db, &cards, Timestamp::now().date())?;
    Ok(CollectionStatistics {
        total_cards: cards.len(),
        due_today: stats.decks.iter().map(|d| d.due).sum(),
        retention: stats.retention,
        decks: stats
            .decks
            .into_iter()
            .map(|d| DeckCounts {
                deck: d.deck_name,
                due: d.due,
                total: d.total,
            })
            .collect(),
    })
}

/// Counts across every collection the caller owns.
///
/// Cheap now that one database holds them all: the connection is opened
/// once per collection rather than once per file, and after the per-user
/// consolidation those are the same file.
pub(super) fn user_stats_for(
    state: &AppState,
    user: Option<&CurrentUser>,
) -> Fallible<UserStatistics> {
    let today = Timestamp::now().date();
    let mut collections = Vec::new();
    for rc in existing_collections_for_user(state, user) {
        let cards = parse_deck(&rc.coll_dir)?.cards;
        let db = open_collection_db(state, &rc)?;
        let due = db.due_today(today)?;
        let seen = db.card_hashes()?;
        let due_today = cards
            .iter()
            .filter(|c| due.contains(&c.hash()) || !seen.contains(&c.hash()))
            .count();
        collections.push(CollectionCounts {
            slug: rc.slug,
            name: rc.name,
            total_cards: cards.len(),
            due_today,
        });
    }
    Ok(UserStatistics { collections })
}

// ── Tools ────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct CollectionArgs {
    /// The collection's slug, as returned by list_collections.
    pub collection: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ReadDeckArgs {
    /// The collection's slug, as returned by list_collections.
    pub collection: String,
    /// The deck file, relative to the collection folder, e.g. `verbs.md` or
    /// `Unit 2/nouns.md`.
    pub deck: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ListCardsArgs {
    /// The collection's slug, as returned by list_collections.
    pub collection: String,
    /// Only cards in this deck, e.g. `verbs.md`.
    pub deck: Option<String>,
    /// Only cards due for review today. A card never reviewed is due.
    pub due_only: Option<bool>,
    /// Only cards whose text contains this, case-insensitively.
    pub query: Option<String>,
    /// At most this many cards (default 50).
    pub limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetCardArgs {
    /// The collection's slug, as returned by list_collections.
    pub collection: String,
    /// The card's content address, from list_cards.
    pub hash: String,
}

#[tool_router(router = read_router, vis = "pub(crate)")]
impl HashcardsMcp {
    #[tool(
        description = "List the collections you can read and write. A collection is a top-level \
                       folder of Markdown card files with its own review schedule; every other \
                       tool takes its slug."
    )]
    async fn list_collections(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<Vec<CollectionSummary>>, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || list_collections_for(&state, caller.current_user().as_ref()))
            .await
            .map(Json)
            .map_err(to_mcp)
    }

    #[tool(
        description = "The decks in one collection, with how many cards each holds and how many \
                       are due today."
    )]
    async fn get_collection(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<CollectionArgs>,
    ) -> Result<Json<CollectionStatistics>, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            collection_stats_for(&state, caller.current_user().as_ref(), &args.collection)
        })
        .await
        .map(Json)
        .map_err(to_mcp)
    }

    #[tool(
        description = "Read a deck's Markdown source, exactly as it is on disk. Cards are \
                       separated by a line containing only `---`; each is `Q:` then `A:`, or \
                       `C:` with `[cloze]` deletions. A file may open with TOML frontmatter \
                       between `---` lines, where `name` overrides the deck's name."
    )]
    async fn read_deck(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<ReadDeckArgs>,
    ) -> Result<String, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            read_deck_for(
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
        description = "List the cards in a collection. Filter by `deck`, by `due_only`, or by \
                       `query`, which matches the card's text case-insensitively. Each card \
                       comes back with its content address (`hash`), which get_card and \
                       update_card take -- and which changes whenever the card's text changes."
    )]
    async fn list_cards(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<ListCardsArgs>,
    ) -> Result<Json<Vec<CardSummary>>, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            list_cards_for(
                &state,
                caller.current_user().as_ref(),
                &args.collection,
                args.deck.as_deref(),
                args.due_only.unwrap_or(false),
                args.query.as_deref(),
                args.limit.unwrap_or(50),
            )
        })
        .await
        .map(Json)
        .map_err(to_mcp)
    }

    #[tool(
        description = "One card in full: its Markdown source, its plain-text front and back, \
                       whether it is due, and its surviving review history. A review the user \
                       undid is not reported: they took it back."
    )]
    async fn get_card(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<GetCardArgs>,
    ) -> Result<Json<CardDetail>, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            get_card_for(
                &state,
                caller.current_user().as_ref(),
                &args.collection,
                &args.hash,
            )
        })
        .await
        .map(Json)
        .map_err(to_mcp)
    }

    #[tool(
        description = "Review statistics for one collection: how many cards, how many due, \
                       recent retention, and per-deck counts. Read-only -- there is no tool to \
                       change a card's schedule."
    )]
    async fn get_collection_stats(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<CollectionArgs>,
    ) -> Result<Json<CollectionStatistics>, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            collection_stats_for(&state, caller.current_user().as_ref(), &args.collection)
        })
        .await
        .map(Json)
        .map_err(to_mcp)
    }

    #[tool(
        description = "Card and due counts across every collection you have, for an overview of \
                       what is waiting to be reviewed."
    )]
    async fn get_user_stats(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<UserStatistics>, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || user_stats_for(&state, caller.current_user().as_ref()))
            .await
            .map(Json)
            .map_err(to_mcp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::serve::cards::CardRoot;
    use crate::cmd::serve::mcp::tools::tests::mcp_fixture;
    use crate::cmd::serve::mcp::tools::tests::other_users_collection;

    #[test]
    fn listing_collections_finds_the_users_own() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let found = list_collections_for(&mcp.state, None)?;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].slug, "Spanish");
        Ok(())
    }

    /// A read tool must not materialise anything -- `collections_for_user`
    /// would write a `.hashcards.toml` into a folder that has none.
    #[test]
    fn listing_collections_does_not_write_an_id_into_a_bare_folder() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let root = CardRoot::for_user(dir.path(), None)?;
        std::fs::create_dir_all(root.path().join("German"))?;
        list_collections_for(&mcp.state, None)?;
        assert!(
            !root.path().join("German/.hashcards.toml").exists(),
            "a read tool created a collection id"
        );
        Ok(())
    }

    #[test]
    fn reading_a_deck_returns_its_source() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let text = read_deck_for(&mcp.state, None, "Spanish", "verbs.md")?;
        assert!(text.contains("Q: hablar"), "{text}");
        Ok(())
    }

    #[test]
    fn reading_a_deck_outside_the_collection_is_refused() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        assert!(read_deck_for(&mcp.state, None, "Spanish", "../../../etc/passwd").is_err());
        assert!(read_deck_for(&mcp.state, None, "Spanish", "nope.md").is_err());
        Ok(())
    }

    #[test]
    fn listing_cards_finds_them_and_filters_by_text() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let all = list_cards_for(&mcp.state, None, "Spanish", None, false, None, 50)?;
        assert_eq!(all.len(), 2);
        let filtered = list_cards_for(&mcp.state, None, "Spanish", None, false, Some("comer"), 50)?;
        assert_eq!(filtered.len(), 1);
        assert!(filtered[0].front.contains("comer"), "{}", filtered[0].front);
        Ok(())
    }

    /// A card nothing has reviewed has no row at all, and is due: it has
    /// never been seen.
    #[test]
    fn a_never_reviewed_card_is_due() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let due = list_cards_for(&mcp.state, None, "Spanish", None, true, None, 50)?;
        assert_eq!(due.len(), 2);
        Ok(())
    }

    #[test]
    fn listing_cards_honours_the_limit() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        assert_eq!(
            list_cards_for(&mcp.state, None, "Spanish", None, false, None, 1)?.len(),
            1
        );
        Ok(())
    }

    #[test]
    fn getting_a_card_returns_its_text_and_an_empty_history() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let cards = list_cards_for(&mcp.state, None, "Spanish", None, false, Some("hablar"), 50)?;
        let card = get_card_for(&mcp.state, None, "Spanish", &cards[0].hash)?;
        assert!(card.front.contains("hablar"), "{}", card.front);
        assert!(card.back.contains("to speak"), "{}", card.back);
        assert!(card.source.contains("Q: hablar"), "{}", card.source);
        assert_eq!(card.kind, "basic");
        assert!(card.reviews.is_empty());
        Ok(())
    }

    /// A stale hash is the normal outcome of editing a card, so the message
    /// has to tell the reader what to do rather than just refusing.
    #[test]
    fn a_stale_hash_tells_the_model_to_read_the_card_again() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let err = get_card_for(&mcp.state, None, "Spanish", &"0".repeat(64)).unwrap_err();
        let msg = err.message();
        assert!(msg.contains("content address"), "unhelpful message: {msg}");
        Ok(())
    }

    #[test]
    fn an_unknown_collection_is_refused_by_name() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let err = read_deck_for(&mcp.state, None, "Klingon", "verbs.md").unwrap_err();
        assert!(err.message().contains("Klingon"), "{}", err.message());
        Ok(())
    }

    #[test]
    fn statistics_count_the_collection_and_its_decks() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let stats = collection_stats_for(&mcp.state, None, "Spanish")?;
        assert_eq!(stats.total_cards, 2);
        assert_eq!(stats.due_today, 2);
        assert_eq!(stats.decks.len(), 1);
        assert_eq!(stats.decks[0].total, 2);

        let user = user_stats_for(&mcp.state, None)?;
        assert_eq!(user.collections.len(), 1);
        assert_eq!(user.collections[0].total_cards, 2);
        Ok(())
    }

    /// The isolation property, tested on its own rather than riding along
    /// on another assertion.
    #[test]
    fn one_users_token_cannot_read_anothers_collection() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let theirs = other_users_collection(&dir)?;

        let mine = list_collections_for(&mcp.state, None)?;
        assert!(
            mine.iter().all(|c| c.slug != theirs),
            "another user's collection is visible"
        );
        assert!(read_deck_for(&mcp.state, None, &theirs, "nouns.md").is_err());
        assert!(list_cards_for(&mcp.state, None, &theirs, None, false, None, 50).is_err());
        assert!(collection_stats_for(&mcp.state, None, &theirs).is_err());
        Ok(())
    }
}
