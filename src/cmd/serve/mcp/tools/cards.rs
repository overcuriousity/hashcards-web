//! Creating, updating and deleting cards.
//!
//! `update_card` goes through `edit_post_inner`, which is what migrates a
//! card's review history to its new hash and re-keys any running drill
//! session. That is the reason this endpoint lives in the server's process
//! rather than a separate binary: a session's card queue is in this
//! process's memory, and nothing outside it could re-key one.

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
use crate::cmd::serve::edit::EditForm;
use crate::cmd::serve::edit::block_end;
use crate::cmd::serve::edit::edit_post_inner;
use crate::cmd::serve::edit::file_mtime_ms;
use crate::cmd::serve::files::save_file;
use crate::cmd::serve::files::user_root;
use crate::cmd::serve::mcp::server::HashcardsMcp;
use crate::cmd::serve::mcp::tools::read::collection_folder;
use crate::cmd::serve::mcp::tools::read::collection_of;
use crate::cmd::serve::mcp::tools::read::to_mcp;
use crate::cmd::serve::state::AppState;
use crate::error::ErrorReport;
use crate::error::Fallible;
use crate::parser::body_start_line;
use crate::parser::parse_deck;
use crate::types::card::Card;
use crate::types::card_hash::CardHash;

/// The separator between two cards in a deck file.
const SEPARATOR: &str = "\n\n---\n\n";

/// Append a card to a deck file.
///
/// Through `save_file`, so the buffer is parsed before it is kept, media is
/// validated, review history is migrated and a running session is re-keyed
/// -- all of it the same code the whole-file editor runs. A buffer that
/// does not parse never reaches the disk.
pub(super) fn create_card_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
    deck: &str,
    card: &str,
) -> Fallible<String> {
    let rc = collection_of(state, user, slug)?;
    let root = user_root(state, user)?;
    let entry = root.resolve_entry(&format!("{}/{deck}", collection_folder(&root, &rc)?))?;
    if !entry.path.is_file() {
        return Err(ErrorReport::new(format!(
            "There is no deck called `{deck}` in `{slug}`. Create it first with create_deck."
        )));
    }
    let existing = std::fs::read_to_string(&entry.path)?;
    let trimmed = existing.trim_end();
    let mut next = String::from(trimmed);
    if !trimmed.is_empty() {
        next.push_str(SEPARATOR);
    }
    next.push_str(card.trim());
    next.push('\n');
    // The mtime is read here rather than carried by the model: `save_file`
    // re-checks it just before the rename, which is what closes the window.
    let mtime = file_mtime_ms(&entry.path)?;
    save_file(state, user, &entry.rel, &next, mtime)?;
    Ok(format!("Added a card to `{deck}`."))
}

/// Where a card is: its file, its line range, and the file's mtime.
fn locate(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
    hash_hex: &str,
) -> Fallible<(std::path::PathBuf, (usize, usize), u64)> {
    let rc = collection_of(state, user, slug)?;
    let hash = CardHash::from_hex(hash_hex)?;
    let cards = parse_deck(&rc.coll_dir)?.cards;
    let card: &Card = cards.iter().find(|c| c.hash() == hash).ok_or_else(|| {
        ErrorReport::new(format!(
            "There is no card with hash `{hash_hex}` in `{slug}`. A card hash is a content \
             address, so it changes whenever the card's text changes -- if you read this card \
             earlier, it has been changed or moved since. Read it again with get_card."
        ))
    })?;
    let path = card.file_path().clone();
    let mtime = file_mtime_ms(&path)?;
    Ok((path, card.range(), mtime))
}

/// Replace one card, keeping its review history.
///
/// `edit_post_inner` is the whole of it: it finds the card by hash,
/// re-checks the file's mtime just before the rename, migrates the review
/// rows from the old hash to the new one, and re-keys any running drill
/// session. Reimplementing any of that here would make a second set of
/// rules that only a model ever exercises.
pub(super) fn update_card_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
    hash_hex: &str,
    card: &str,
) -> Fallible<String> {
    let owner = user.map(|u| u.email.to_lowercase());
    let (_, _, mtime) = locate(state, user, slug, hash_hex)?;
    let form = EditForm {
        new_text: card.trim().to_string(),
        mtime_ms: mtime.to_string(),
        return_to: None,
    };
    let outcome = edit_post_inner(state, slug, hash_hex, form, owner.as_deref())?;
    let mut msg = String::from("Card updated.");
    if outcome.skipped > 0 {
        msg.push_str(&format!(
            " {} card(s) could not be matched to their previous review history and start fresh.",
            outcome.skipped
        ));
    }
    if outcome.session.renamed > 0 || outcome.session.dropped > 0 {
        msg.push_str(" A drill session in progress was updated.");
    }
    Ok(msg)
}

/// Take a card's block out of its file.
///
/// Through `save_file` like every other write, so the remaining buffer is
/// parsed before it is kept and a running session is re-keyed.
pub(super) fn delete_card_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
    hash_hex: &str,
) -> Fallible<String> {
    let (path, range, mtime) = locate(state, user, slug, hash_hex)?;
    let content = std::fs::read_to_string(&path)?;
    let remaining = remove_card_lines(&content, range);

    let root = user_root(state, user)?;
    let rel = path
        .strip_prefix(root.path().canonicalize()?)
        .or_else(|_| path.strip_prefix(root.path()))
        .map_err(|_| ErrorReport::new("That card is not in your card folder."))?
        .to_string_lossy()
        .replace('\\', "/");
    save_file(state, user, &rel, &remaining, mtime)?;
    Ok("Card deleted.".to_string())
}

/// The file without the card at `range`, and without the separator that
/// joined it to its neighbours -- leaving one behind would make an empty
/// card, which does not parse.
///
/// By line range, not by matching the block's text. Splitting the whole
/// file on `\n---\n` got two things wrong: TOML frontmatter is delimited
/// exactly the same way, so deleting the last card of a named deck carried
/// the closing `---` off with it and left a file that no longer parsed at
/// all; and two byte-identical blocks both matched, so deleting one card
/// removed both. `body_start_line` is what keeps the backward scan off the
/// frontmatter.
fn remove_card_lines(content: &str, range: (usize, usize)) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let body = body_start_line(content);
    let blank = |i: usize| lines.get(i).is_some_and(|l| l.trim().is_empty());
    let separator = |i: usize| lines.get(i).is_some_and(|l| l.trim() == "---");

    let mut start = range.0;
    let mut end = block_end(&lines, range);

    // The separator *after* the card, with the blank lines around it.
    let mut after = end;
    while blank(after) {
        after += 1;
    }
    if separator(after) {
        end = after + 1;
        while blank(end) {
            end += 1;
        }
    } else {
        // There is none: this was the last card in the file, so the
        // separator to take is the one before it -- but never back past the
        // frontmatter, whose closing delimiter looks just like one.
        let mut before = start;
        while before > body && blank(before - 1) {
            before -= 1;
        }
        if before > body && separator(before - 1) {
            start = before - 1;
            while start > body && blank(start - 1) {
                start -= 1;
            }
        }
    }

    let start = start.min(lines.len());
    let end = end.max(start).min(lines.len());
    let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
    kept.extend_from_slice(&lines[..start]);
    kept.extend_from_slice(&lines[end..]);
    let mut out = kept.join("\n");
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

// ── Tools ────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct CreateCardArgs {
    /// The collection's slug, as returned by list_collections.
    pub collection: String,
    /// The deck file, relative to the collection folder, e.g. `verbs.md`.
    pub deck: String,
    /// The card's text. `Q:` then `A:` for a basic card, or `C:` with
    /// `[cloze]` deletions. Do not include the `---` separator.
    pub card: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct UpdateCardArgs {
    /// The collection's slug, as returned by list_collections.
    pub collection: String,
    /// The card's content address as it is NOW, from list_cards or get_card.
    pub hash: String,
    /// The card's new text, in the same syntax.
    pub card: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct DeleteCardArgs {
    /// The collection's slug, as returned by list_collections.
    pub collection: String,
    /// The card's content address, from list_cards or get_card.
    pub hash: String,
}

#[tool_router(router = card_router, vis = "pub(crate)")]
impl HashcardsMcp {
    #[tool(
        description = "Add a card to a deck. Card syntax: `Q:` then `A:` for a basic card, or \
                       `C:` with `[cloze]` deletions for a cloze card, where each deletion \
                       becomes its own card. Do not include the `---` separator -- it is added \
                       for you. Text that does not parse is refused and the deck is left as it \
                       was."
    )]
    async fn create_card(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<CreateCardArgs>,
    ) -> Result<String, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            create_card_for(
                &state,
                caller.current_user().as_ref(),
                &args.collection,
                &args.deck,
                &args.card,
            )
        })
        .await
        .map_err(to_mcp)
    }

    #[tool(
        description = "Replace a card, keeping its review history. `hash` is the card's content \
                       address as it is NOW -- editing a card changes its hash, so if this \
                       fails saying the card has changed, read it again with get_card rather \
                       than retrying. Card syntax: `Q:`/`A:`, or `C:` with `[cloze]` deletions."
    )]
    async fn update_card(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<UpdateCardArgs>,
    ) -> Result<String, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            update_card_for(
                &state,
                caller.current_user().as_ref(),
                &args.collection,
                &args.hash,
                &args.card,
            )
        })
        .await
        .map_err(to_mcp)
    }

    #[tool(
        description = "Remove a card from its deck. Its review history stays in the database, so \
                       adding the same card back restores its schedule -- a card hash is a \
                       content address, so the identical text is the identical card."
    )]
    async fn delete_card(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<DeleteCardArgs>,
    ) -> Result<String, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            delete_card_for(
                &state,
                caller.current_user().as_ref(),
                &args.collection,
                &args.hash,
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
    use crate::cmd::serve::mcp::tools::read::list_cards_for;
    use crate::cmd::serve::mcp::tools::tests::mcp_fixture;
    use crate::cmd::serve::mcp::tools::tests::other_users_collection;
    use crate::cmd::serve::mcp::tools::tests::seed_session;
    use crate::cmd::serve::mcp::tools::tests::session_card_hashes;

    fn hash_of(mcp: &HashcardsMcp, needle: &str) -> Fallible<String> {
        let cards = list_cards_for(&mcp.state, None, "Spanish", None, false, Some(needle), 50)?;
        Ok(cards
            .first()
            .map(|c| c.hash.clone())
            .expect("the fixture card"))
    }

    #[test]
    fn a_created_card_is_in_the_file_and_in_the_listing() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        create_card_for(
            &mcp.state,
            None,
            "Spanish",
            "verbs.md",
            "Q: beber\nA: to drink\n",
        )?;
        let cards = list_cards_for(&mcp.state, None, "Spanish", None, false, Some("beber"), 50)?;
        assert_eq!(cards.len(), 1);
        assert_eq!(
            list_cards_for(&mcp.state, None, "Spanish", None, false, None, 50)?.len(),
            3,
            "the other cards were disturbed"
        );
        Ok(())
    }

    /// A buffer that does not parse never stays on disk.
    #[test]
    fn a_created_card_that_does_not_parse_is_refused_and_changes_nothing() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let path = CardRoot::for_user(dir.path(), None)?
            .path()
            .join("Spanish/verbs.md");
        let before = std::fs::read_to_string(&path)?;
        assert!(
            create_card_for(&mcp.state, None, "Spanish", "verbs.md", "A: no question\n").is_err()
        );
        assert_eq!(std::fs::read_to_string(&path)?, before);
        Ok(())
    }

    /// The property the whole edit path exists to preserve.
    #[test]
    fn an_updated_card_keeps_its_review_history() -> Fallible<()> {
        use crate::cmd::serve::cards::collection_id;
        use crate::cmd::serve::cards::user_db_path;
        use crate::types::timestamp::Timestamp;
        use crate::user_db::UserDatabase;

        let (dir, mcp) = mcp_fixture()?;
        let root = CardRoot::for_user(dir.path(), None)?;
        let id = collection_id(&root.path().join("Spanish"))?;
        let db = UserDatabase::open(&user_db_path(&root, &dir.path().join("db"))?)?;

        let old_hex = hash_of(&mcp, "hablar")?;
        let old = CardHash::from_hex(&old_hex)?;
        db.collection(id.clone())
            .insert_card(old, Timestamp::now())?;

        update_card_for(
            &mcp.state,
            None,
            "Spanish",
            &old_hex,
            "Q: hablar\nA: to speak, to talk\n",
        )?;

        let new_hex = hash_of(&mcp, "to talk")?;
        assert_ne!(new_hex, old_hex, "the hash must change with the text");
        let new = CardHash::from_hex(&new_hex)?;
        assert!(
            db.collection(id).card_hashes()?.contains(&new),
            "the review history did not follow the card"
        );
        Ok(())
    }

    /// The reason this endpoint is in the server's process and not a
    /// separate binary: a session's card queue lives in this process's
    /// memory, and an edit has to re-key it or the session grades against
    /// hashes that no longer exist.
    #[test]
    fn editing_a_card_rekeys_a_running_drill_session() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let root = CardRoot::for_user(dir.path(), None)?;
        let folder = root.path().join("Spanish");
        let cards = parse_deck(&folder)?.cards;
        seed_session(&mcp.state, dir.path(), &folder, "Spanish", cards)?;

        let old_hex = hash_of(&mcp, "hablar")?;
        assert!(session_card_hashes(&mcp.state, "Spanish").contains(&old_hex));

        update_card_for(
            &mcp.state,
            None,
            "Spanish",
            &old_hex,
            "Q: hablar\nA: to speak, to talk\n",
        )?;

        let queued = session_card_hashes(&mcp.state, "Spanish");
        assert!(
            !queued.contains(&old_hex),
            "the session still holds a hash that no longer exists: {queued:?}"
        );
        assert!(
            queued.contains(&hash_of(&mcp, "to talk")?),
            "the session did not follow the card: {queued:?}"
        );
        Ok(())
    }

    #[test]
    fn a_stale_hash_tells_the_model_to_read_the_card_again() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let err = update_card_for(&mcp.state, None, "Spanish", &"0".repeat(64), "Q: x\nA: y\n")
            .unwrap_err();
        assert!(
            err.message().contains("content address"),
            "unhelpful message: {}",
            err.message()
        );
        Ok(())
    }

    #[test]
    fn a_deleted_card_leaves_the_others_alone() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let hex = hash_of(&mcp, "hablar")?;
        delete_card_for(&mcp.state, None, "Spanish", &hex)?;
        let left = list_cards_for(&mcp.state, None, "Spanish", None, false, None, 50)?;
        assert_eq!(left.len(), 1);
        assert!(left[0].front.contains("comer"), "{}", left[0].front);
        Ok(())
    }

    #[test]
    fn another_users_collection_is_refused() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let theirs = other_users_collection(&dir)?;
        assert!(create_card_for(&mcp.state, None, &theirs, "nouns.md", "Q: a\nA: b\n").is_err());
        assert!(
            update_card_for(&mcp.state, None, &theirs, &"0".repeat(64), "Q: a\nA: b\n").is_err()
        );
        assert!(delete_card_for(&mcp.state, None, &theirs, &"0".repeat(64)).is_err());
        Ok(())
    }

    /// Regression: `remove_block` split the whole file on `\n---\n`, which
    /// is also what closes TOML frontmatter. Deleting the only card of a
    /// named deck left the opening `---` with nothing to close it, so the
    /// re-parse failed, `save_file` reverted, and the card could never be
    /// deleted at all -- the tool answered "Not saved -- Frontmatter opening
    /// '---' found but no closing '---'".
    #[test]
    fn the_last_card_of_a_deck_with_frontmatter_can_be_deleted() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let root = CardRoot::for_user(dir.path(), None)?;
        let path = root.path().join("Spanish/nouns.md");
        std::fs::write(
            &path,
            "---\nname = \"Nouns\"\n---\n\nQ: el libro\nA: the book\n",
        )?;

        let hex = list_cards_for(&mcp.state, None, "Spanish", None, false, Some("libro"), 50)?[0]
            .hash
            .clone();
        delete_card_for(&mcp.state, None, "Spanish", &hex)?;

        let left = std::fs::read_to_string(&path)?;
        assert!(
            left.contains("name = \"Nouns\""),
            "the frontmatter was destroyed: {left:?}"
        );
        // It still parses, and the card is gone.
        let cards = parse_deck(&root.path().join("Spanish"))?.cards;
        assert!(!cards.iter().any(|c| c.hash().to_string() == hex));
        Ok(())
    }

    /// Frontmatter is not the only thing the split got wrong: two
    /// byte-identical blocks both matched the text of the card being
    /// deleted, so deleting one removed both.
    #[test]
    fn deleting_one_of_two_identical_cards_removes_one_block() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let root = CardRoot::for_user(dir.path(), None)?;
        let path = root.path().join("Spanish/verbs.md");
        std::fs::write(
            &path,
            "Q: hablar\nA: to speak\n\n---\n\nQ: hablar\nA: to speak\n",
        )?;

        let hex = list_cards_for(&mcp.state, None, "Spanish", None, false, Some("hablar"), 50)?[0]
            .hash
            .clone();
        delete_card_for(&mcp.state, None, "Spanish", &hex)?;

        let left = std::fs::read_to_string(&path)?;
        assert_eq!(
            left.matches("hablar").count(),
            1,
            "both copies were removed: {left:?}"
        );
        Ok(())
    }

    /// Deleting a card from the middle takes its separator with it: leaving
    /// one behind would make an empty card, which does not parse.
    #[test]
    fn deleting_a_middle_card_leaves_its_neighbours_parseable() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let root = CardRoot::for_user(dir.path(), None)?;
        let path = root.path().join("Spanish/verbs.md");
        std::fs::write(
            &path,
            "Q: uno\nA: one\n\n---\n\nQ: dos\nA: two\n\n---\n\nQ: tres\nA: three\n",
        )?;

        let hex = list_cards_for(&mcp.state, None, "Spanish", None, false, Some("dos"), 50)?[0]
            .hash
            .clone();
        delete_card_for(&mcp.state, None, "Spanish", &hex)?;

        let left = std::fs::read_to_string(&path)?;
        assert!(!left.contains("dos"), "{left:?}");
        // Two cards left, and the file still parses as two.
        assert_eq!(parse_deck(&root.path().join("Spanish"))?.cards.len(), 2);
        Ok(())
    }

    /// `create_card` resolved its deck out of the slug too, so it answered
    /// "There is no deck called `facts.md`. Create it first" for a deck
    /// that was right there.
    #[test]
    fn a_card_can_be_created_in_a_collection_whose_name_is_not_slug_shaped() -> Fallible<()> {
        use crate::cmd::serve::mcp::tools::tests::spaced_collection;

        let (dir, mcp) = mcp_fixture()?;
        let slug = spaced_collection(&dir)?;

        create_card_for(&mcp.state, None, &slug, "facts.md", "Q: e\nA: 2.71828\n")?;

        assert_eq!(
            list_cards_for(&mcp.state, None, &slug, None, false, None, 50)?.len(),
            2
        );
        // And it went into the collection's own folder, not a new one named
        // after the slug.
        let root = CardRoot::for_user(dir.path(), None)?;
        assert!(!root.path().join("Exam-revision").exists());
        Ok(())
    }
}
