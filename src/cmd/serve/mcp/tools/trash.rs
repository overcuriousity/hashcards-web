//! Listing and restoring from the trash.
//!
//! **There is deliberately no purge tool.** Emptying the trash is a human
//! action in the web interface, and that is what makes "the model cannot
//! destroy anything irrecoverably" a property of this design rather than a
//! hope. A test below asserts that no tool in the whole surface is named
//! like one, so a tool added in a hurry cannot quietly take the property
//! away.

use rmcp::ErrorData;
use rmcp::RoleServer;
use rmcp::handler::server::wrapper::Json;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::schemars;
use rmcp::schemars::JsonSchema;
use rmcp::service::RequestContext;
use rmcp::tool;
use rmcp::tool_router;
use serde::Deserialize;
use serde::Serialize;

use crate::cmd::run_blocking;
use crate::cmd::serve::auth::CurrentUser;
use crate::cmd::serve::files::user_root;
use crate::cmd::serve::files::user_root_readonly;
use crate::cmd::serve::mcp::server::HashcardsMcp;
use crate::cmd::serve::mcp::tools::read::to_mcp;
use crate::cmd::serve::state::AppState;
use crate::cmd::serve::trash::TrashId;
use crate::cmd::serve::trash::list_trash;
use crate::cmd::serve::trash::restore_from_trash;
use crate::error::Fallible;
use crate::error::fail;

#[derive(Debug, Serialize, JsonSchema)]
pub struct TrashedItem {
    /// What restore_from_trash takes.
    pub id: String,
    /// "file", "folder" or "collection".
    pub kind: String,
    /// Where it was when it was deleted.
    pub original_path: String,
    pub deleted_at: String,
}

fn data_dir(state: &AppState) -> Fallible<std::path::PathBuf> {
    match &state.config.data_dir {
        Some(d) => Ok(d.clone()),
        None => fail("No data directory is configured, so there is no trash."),
    }
}

pub(super) fn list_trash_for(
    state: &AppState,
    user: Option<&CurrentUser>,
) -> Fallible<Vec<TrashedItem>> {
    let data_dir = data_dir(state)?;
    // Read-only: listing the trash must not materialise a card folder.
    let root = user_root_readonly(state, user)?;
    Ok(list_trash(&data_dir, root.tree_name()?)?
        .into_iter()
        .map(|e| TrashedItem {
            id: e.id.to_string(),
            kind: e.kind.as_str().to_string(),
            original_path: e.original_path,
            deleted_at: e.deleted_at.to_string(),
        })
        .collect())
}

pub(super) fn restore_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    raw_id: &str,
) -> Fallible<String> {
    let data_dir = data_dir(state)?;
    let id = TrashId::parse(raw_id)?;
    let root = user_root(state, user)?;
    let rel = restore_from_trash(&data_dir, &root, &id)?;
    Ok(format!("Restored `{rel}`."))
}

// ── Tools ────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct RestoreArgs {
    /// The trashed item's id, from list_trash.
    pub id: String,
}

#[tool_router(router = trash_router, vis = "pub(crate)")]
impl HashcardsMcp {
    #[tool(
        description = "List what is in the user's trash. Anything deleted through these tools or \
                       through the web interface is here until the user empties it, and can be \
                       restored."
    )]
    async fn list_trash(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<Vec<TrashedItem>>, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || list_trash_for(&state, caller.current_user().as_ref()))
            .await
            .map(Json)
            .map_err(to_mcp)
    }

    #[tool(
        description = "Restore something from the trash to where it was. A restored collection \
                       gets its review history back. This fails if something has since taken \
                       the same path -- move that out of the way first. There is no tool to \
                       empty the trash: only the user can do that, from the web interface."
    )]
    async fn restore_from_trash(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<RestoreArgs>,
    ) -> Result<String, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || restore_for(&state, caller.current_user().as_ref(), &args.id))
            .await
            .map_err(to_mcp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::serve::cards::CardRoot;
    use crate::cmd::serve::cards::collection_id;
    use crate::cmd::serve::mcp::tools::collections::delete_collection_for;
    use crate::cmd::serve::mcp::tools::decks::delete_deck_for;
    use crate::cmd::serve::mcp::tools::read::list_collections_for;
    use crate::cmd::serve::mcp::tools::tests::mcp_fixture;

    #[test]
    fn the_trash_lists_what_was_deleted() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        delete_deck_for(&mcp.state, None, "Spanish", "verbs.md")?;
        let entries = list_trash_for(&mcp.state, None)?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].original_path, "Spanish/verbs.md");
        assert_eq!(entries[0].kind, "file");
        Ok(())
    }

    #[test]
    fn an_empty_trash_lists_nothing() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        assert!(list_trash_for(&mcp.state, None)?.is_empty());
        Ok(())
    }

    /// The whole point of leaving the review rows behind on delete.
    #[test]
    fn restoring_a_collection_brings_its_history_back() -> Fallible<()> {
        use crate::cmd::serve::cards::user_db_path;
        use crate::types::card_hash::CardHash;
        use crate::types::timestamp::Timestamp;
        use crate::user_db::UserDatabase;

        let (dir, mcp) = mcp_fixture()?;
        let root = CardRoot::for_user(dir.path(), None)?;
        let id = collection_id(&root.path().join("Spanish"))?;
        let db = UserDatabase::open(&user_db_path(&root, &dir.path().join("db"))?)?;
        let hash = CardHash::hash_bytes(b"a card");
        db.collection(id.clone())
            .insert_card(hash, Timestamp::now())?;

        delete_collection_for(&mcp.state, None, "Spanish")?;
        let entries = list_trash_for(&mcp.state, None)?;
        restore_for(&mcp.state, None, &entries[0].id)?;

        assert!(
            list_collections_for(&mcp.state, None)?
                .iter()
                .any(|c| c.slug == "Spanish")
        );
        assert_eq!(collection_id(&root.path().join("Spanish"))?, id);
        assert!(db.collection(id).card_hashes()?.contains(&hash));
        assert!(list_trash_for(&mcp.state, None)?.is_empty());
        Ok(())
    }

    /// Restoring must not overwrite whatever took the name in the meantime
    /// -- that would destroy something without trashing it.
    #[test]
    fn restoring_onto_an_occupied_path_is_refused_and_keeps_the_entry() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        delete_deck_for(&mcp.state, None, "Spanish", "verbs.md")?;
        let root = CardRoot::for_user(dir.path(), None)?;
        std::fs::write(root.path().join("Spanish/verbs.md"), "Q: x\nA: y\n")?;

        let entries = list_trash_for(&mcp.state, None)?;
        assert!(restore_for(&mcp.state, None, &entries[0].id).is_err());
        assert_eq!(list_trash_for(&mcp.state, None)?.len(), 1);
        assert_eq!(
            std::fs::read_to_string(root.path().join("Spanish/verbs.md"))?,
            "Q: x\nA: y\n"
        );
        Ok(())
    }

    #[test]
    fn a_crafted_trash_id_is_refused() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        assert!(restore_for(&mcp.state, None, "../../etc/passwd").is_err());
        assert!(restore_for(&mcp.state, None, "").is_err());
        Ok(())
    }

    #[test]
    fn one_user_cannot_see_anothers_trash() -> Fallible<()> {
        use crate::cmd::serve::trash::TrashKind;
        use crate::cmd::serve::trash::move_to_trash;
        use crate::types::timestamp::Timestamp;

        let (dir, mcp) = mcp_fixture()?;
        let theirs = CardRoot::for_user(dir.path(), Some("you@example.com"))?;
        std::fs::create_dir_all(theirs.path().join("German"))?;
        move_to_trash(
            dir.path(),
            &theirs,
            "German",
            TrashKind::Folder,
            None,
            Timestamp::now(),
        )?;
        assert!(list_trash_for(&mcp.state, None)?.is_empty());
        Ok(())
    }

    /// The safety property of this whole feature, asserted rather than
    /// hoped for. Emptying the trash is a human action in the web
    /// interface; if a tool ever appears that does it, this fails.
    #[test]
    fn no_tool_in_the_whole_surface_destroys_anything() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let names = mcp.tool_names();
        for name in &names {
            let lower = name.to_lowercase();
            for forbidden in ["purge", "empty", "destroy", "erase", "wipe"] {
                assert!(
                    !lower.contains(forbidden),
                    "`{name}` looks like it destroys data. Emptying the trash is a human action \
                     in the web interface, and that is what makes this feature safe to hand a \
                     model a write token."
                );
            }
        }
        assert!(names.iter().any(|n| n == "restore_from_trash"));
        assert!(names.iter().any(|n| n == "list_trash"));
        Ok(())
    }

    /// The design settled on twenty-three tools. A mismatch means one was
    /// forgotten or one was invented.
    #[test]
    fn the_tool_surface_is_the_one_the_design_settled_on() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let mut names = mcp.tool_names();
        names.sort();
        let mut expected = vec![
            // Read
            "list_collections",
            "get_collection",
            "read_deck",
            "list_cards",
            "get_card",
            "get_collection_stats",
            "get_user_stats",
            // Cards
            "create_card",
            "update_card",
            "delete_card",
            // Decks
            "create_deck",
            "write_deck",
            "move_decks",
            "delete_deck",
            // Collections
            "create_collection",
            "rename_collection",
            "delete_collection",
            "set_collection_scheduling",
            // Saved decks
            "list_saved_decks",
            "set_saved_deck",
            "delete_saved_deck",
            // Trash
            "list_trash",
            "restore_from_trash",
        ];
        expected.sort_unstable();
        assert_eq!(names, expected);
        assert_eq!(names.len(), 23);
        Ok(())
    }
}
