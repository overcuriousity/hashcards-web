//! Creating, renaming, deleting collections and setting their scheduling.
//!
//! Each of these is the file manager's own function with the collection
//! resolved first, so the slug-collision check, the live-session guard and
//! `CardRoot`'s path checking all apply to a model exactly as they apply to
//! a browser.

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
use crate::cmd::serve::cards::write_collection_overrides;
use crate::cmd::serve::files::create_entry;
use crate::cmd::serve::files::delete_entry;
use crate::cmd::serve::files::rename_entry;
use crate::cmd::serve::mcp::server::HashcardsMcp;
use crate::cmd::serve::mcp::tools::read::collection_of;
use crate::cmd::serve::mcp::tools::read::to_mcp;
use crate::cmd::serve::state::AppState;
use crate::error::Fallible;
use crate::types::performance::DesiredRetention;
use crate::types::performance::MaxInterval;

pub(super) fn create_collection_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    name: &str,
) -> Fallible<String> {
    // A collection is a top-level folder, so the parent is the root.
    // `create_entry` gives it an id immediately, which is what its review
    // history will be keyed by.
    create_entry(state, user, "", name, true)
}

pub(super) fn rename_collection_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
    name: &str,
) -> Fallible<String> {
    collection_of(state, user, slug)?;
    // The id lives in the folder and travels with it, so the review
    // history follows the rename.
    rename_entry(state, user, slug, name)
}

pub(super) fn delete_collection_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
) -> Fallible<String> {
    collection_of(state, user, slug)?;
    delete_entry(state, user, slug)
}

/// Set or clear a collection's scheduling overrides.
///
/// This changes how *future* intervals are computed. It does not move any
/// card's current due date: the schedule itself is not writable over MCP.
pub(super) fn set_scheduling_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
    retention: Option<f64>,
    max_interval_days: Option<f64>,
) -> Fallible<String> {
    let rc = collection_of(state, user, slug)?;
    // Validated through the same newtypes the config parser uses, so an
    // out-of-range value is refused with the message that already explains
    // the range rather than a second wording of it.
    let retention = match retention {
        Some(v) => Some(DesiredRetention::new(v)?),
        None => None,
    };
    let max_interval = match max_interval_days {
        Some(v) => Some(MaxInterval::new(v)?),
        None => None,
    };
    write_collection_overrides(&rc.coll_dir, retention, max_interval)?;
    Ok(format!("Scheduling updated for `{slug}`."))
}

// ── Tools ────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct CreateCollectionArgs {
    /// The collection's name. It becomes a URL slug, which must not collide
    /// with an existing collection or saved deck.
    pub name: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct RenameCollectionArgs {
    /// The collection's current slug, as returned by list_collections.
    pub collection: String,
    /// Its new name.
    pub name: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct CollectionOnlyArgs {
    /// The collection's slug, as returned by list_collections.
    pub collection: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SchedulingArgs {
    /// The collection's slug, as returned by list_collections.
    pub collection: String,
    /// The fraction of cards the scheduler aims to have you remember when
    /// they come up. Higher means shorter intervals and more reviews. Omit
    /// to fall back to the instance default.
    pub desired_retention: Option<f64>,
    /// The longest interval the scheduler will give a card, in days. Omit
    /// to fall back to the instance default.
    pub max_interval_days: Option<f64>,
}

#[tool_router(router = collection_router, vis = "pub(crate)")]
impl HashcardsMcp {
    #[tool(
        description = "Create a collection: a top-level folder of decks with its own review \
                       schedule. The name becomes a URL slug, which must not collide with an \
                       existing collection or saved deck."
    )]
    async fn create_collection(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<CreateCollectionArgs>,
    ) -> Result<String, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            create_collection_for(&state, caller.current_user().as_ref(), &args.name)
        })
        .await
        .map_err(to_mcp)
    }

    #[tool(
        description = "Rename a collection. Its review history follows it: the schedule is keyed \
                       by an id stored inside the folder, not by its name."
    )]
    async fn rename_collection(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<RenameCollectionArgs>,
    ) -> Result<String, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            rename_collection_for(
                &state,
                caller.current_user().as_ref(),
                &args.collection,
                &args.name,
            )
        })
        .await
        .map_err(to_mcp)
    }

    #[tool(
        description = "Delete a collection and every deck in it. It goes to the user's trash \
                       with its review history intact and can be restored with \
                       restore_from_trash; only the user can empty the trash, and only that \
                       erases the history."
    )]
    async fn delete_collection(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<CollectionOnlyArgs>,
    ) -> Result<String, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            delete_collection_for(&state, caller.current_user().as_ref(), &args.collection)
        })
        .await
        .map_err(to_mcp)
    }

    #[tool(
        description = "Set a collection's scheduling in place of the instance defaults. This \
                       changes how future intervals are computed; it does not move any card's \
                       current due date. Omit a value to fall back to the default."
    )]
    async fn set_collection_scheduling(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<SchedulingArgs>,
    ) -> Result<String, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            set_scheduling_for(
                &state,
                caller.current_user().as_ref(),
                &args.collection,
                args.desired_retention,
                args.max_interval_days,
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
    use crate::cmd::serve::cards::collection_overrides;
    use crate::cmd::serve::mcp::tools::read::list_collections_for;
    use crate::cmd::serve::mcp::tools::tests::mcp_fixture;
    use crate::cmd::serve::mcp::tools::tests::other_users_collection;
    use crate::cmd::serve::trash::list_trash;

    #[test]
    fn a_created_collection_is_listed_and_has_an_id() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        create_collection_for(&mcp.state, None, "German")?;
        assert!(
            list_collections_for(&mcp.state, None)?
                .iter()
                .any(|c| c.slug == "German")
        );
        let root = CardRoot::for_user(dir.path(), None)?;
        assert!(root.path().join("German/.hashcards.toml").is_file());
        Ok(())
    }

    /// The id is what the review rows are keyed by, so a rename that
    /// changed it would silently start the history over.
    #[test]
    fn a_renamed_collection_keeps_its_id() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let root = CardRoot::for_user(dir.path(), None)?;
        let before = collection_id(&root.path().join("Spanish"))?;
        rename_collection_for(&mcp.state, None, "Spanish", "Castellano")?;
        let after = collection_id(&root.path().join("Castellano"))?;
        assert_eq!(before, after);
        Ok(())
    }

    #[test]
    fn a_colliding_name_is_refused_and_says_what_it_collides_with() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let err = create_collection_for(&mcp.state, None, "Spanish").unwrap_err();
        assert!(err.message().contains("Spanish"), "{}", err.message());
        Ok(())
    }

    #[test]
    fn a_deleted_collection_is_in_the_trash_with_its_rows_intact() -> Fallible<()> {
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

        assert!(list_collections_for(&mcp.state, None)?.is_empty());
        assert_eq!(list_trash(dir.path(), "default")?.len(), 1);
        assert!(
            db.collection(id).card_hashes()?.contains(&hash),
            "the rows a restore would need are gone"
        );
        Ok(())
    }

    #[test]
    fn scheduling_overrides_round_trip_and_keep_the_id() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let root = CardRoot::for_user(dir.path(), None)?;
        let before = collection_id(&root.path().join("Spanish"))?;

        set_scheduling_for(&mcp.state, None, "Spanish", Some(0.85), Some(365.0))?;

        let overrides = collection_overrides(&root.path().join("Spanish"));
        assert_eq!(overrides.retention.map(|r| r.into_inner()), Some(0.85));
        assert_eq!(overrides.max_interval.map(|m| m.into_inner()), Some(365.0));
        assert_eq!(
            collection_id(&root.path().join("Spanish"))?,
            before,
            "rewriting the overrides lost the collection's id"
        );
        Ok(())
    }

    #[test]
    fn clearing_an_override_falls_back_to_the_default() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let root = CardRoot::for_user(dir.path(), None)?;
        set_scheduling_for(&mcp.state, None, "Spanish", Some(0.85), None)?;
        set_scheduling_for(&mcp.state, None, "Spanish", None, None)?;
        let overrides = collection_overrides(&root.path().join("Spanish"));
        assert!(overrides.retention.is_none());
        Ok(())
    }

    /// Refused through the same newtype the config parser uses, so the
    /// message explaining the range is written once.
    #[test]
    fn a_scheduling_value_out_of_range_is_refused() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        assert!(set_scheduling_for(&mcp.state, None, "Spanish", Some(2.0), None).is_err());
        assert!(set_scheduling_for(&mcp.state, None, "Spanish", Some(0.1), None).is_err());
        assert!(set_scheduling_for(&mcp.state, None, "Spanish", None, Some(0.0)).is_err());
        Ok(())
    }

    #[test]
    fn another_users_collection_is_refused() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let theirs = other_users_collection(&dir)?;
        assert!(rename_collection_for(&mcp.state, None, &theirs, "Deutsch").is_err());
        assert!(delete_collection_for(&mcp.state, None, &theirs).is_err());
        assert!(set_scheduling_for(&mcp.state, None, &theirs, Some(0.9), None).is_err());
        Ok(())
    }
}
