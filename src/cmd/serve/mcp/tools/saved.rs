//! Saved decks: a named selection of decks drawn from any of the caller's
//! collections, drilled together as one session.
//!
//! These live in `hashcards.toml`, not in the card tree, and the running
//! server keeps its own resolved copy. Both have to change together and in
//! that order -- persist, then swap, under one lock -- or a failed write
//! leaves the server disagreeing with its own config file until somebody
//! restarts it. This is `deck_add_handler`'s ordering, and the reason it is
//! not rearranged here.
//!
//! A saved deck holds no cards of its own: deleting one never touches a
//! card or its review history.

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
use crate::cmd::serve::config::DeckMember;
use crate::cmd::serve::decks::ResolvedCustomDeck;
use crate::cmd::serve::decks::check_deck_slug_collisions;
use crate::cmd::serve::decks::entries_from;
use crate::cmd::serve::decks::owned_collections;
use crate::cmd::serve::decks::persist_custom_decks;
use crate::cmd::serve::decks::slug_for_deck;
use crate::cmd::serve::mcp::server::HashcardsMcp;
use crate::cmd::serve::mcp::tools::read::to_mcp;
use crate::cmd::serve::state::AppState;
use crate::error::Fallible;
use crate::error::fail;

#[derive(Debug, Serialize, JsonSchema)]
pub struct SavedDeck {
    pub name: String,
    /// What the deck is addressed by, like a collection's slug.
    pub slug: String,
    /// `"{collection-slug}/{deck-name}"` pairs.
    pub members: Vec<String>,
}

fn owner_key(user: Option<&CurrentUser>) -> Option<String> {
    user.map(|u| u.email.to_lowercase())
}

pub(super) fn list_saved_decks_for(
    state: &AppState,
    user: Option<&CurrentUser>,
) -> Fallible<Vec<SavedDeck>> {
    let owner = owner_key(user);
    Ok(state
        .custom_decks
        .lock()
        .iter()
        .filter(|d| d.owner.as_deref() == owner.as_deref())
        .map(|d| SavedDeck {
            name: d.name.clone(),
            slug: d.slug.clone(),
            members: d.members.iter().map(|m| m.encode()).collect(),
        })
        .collect())
}

/// Create or replace a saved deck.
pub(super) fn set_saved_deck_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    name: &str,
    members: &[String],
) -> Fallible<String> {
    let owner = owner_key(user);
    let name = name.trim().to_string();
    if name.is_empty() {
        return fail("Give the deck a name.");
    }
    if members.is_empty() {
        return fail(
            "A saved deck needs at least one member, written as `{collection-slug}/{deck-name}`.",
        );
    }

    // Every member must name a collection the caller actually owns, or a
    // saved deck could be used to read another user's cards.
    let owned = owned_collections(state, owner.as_deref());
    let mut parsed = Vec::new();
    for raw in members {
        let Some(member) = DeckMember::parse(raw) else {
            return fail(format!(
                "`{raw}` is not a deck reference. Write it as `{{collection-slug}}/{{deck-name}}`."
            ));
        };
        if !owned.iter().any(|c| c.slug == member.collection_slug) {
            return fail(format!(
                "You have no collection called `{}`.",
                member.collection_slug
            ));
        }
        parsed.push(member);
    }

    let Some(config_path) = state.config_path.lock().clone() else {
        return fail(
            "Saved decks cannot be stored: no config file is in use. hashcards-web was started \
             without one.",
        );
    };

    let new_deck = ResolvedCustomDeck {
        name: name.clone(),
        slug: slug_for_deck(&name, owner.as_deref()),
        owner: owner.clone(),
        members: parsed,
    };

    // Replace, mutate and persist under one lock, so two concurrent writes
    // cannot produce a config missing each other's decks (BUG-39). The swap
    // happens only after the write succeeds.
    let mut guard = state.custom_decks.lock();
    let mut updated = guard.clone();
    let replaced = updated
        .iter()
        .any(|d| d.name == name && d.owner.as_deref() == owner.as_deref());
    updated.retain(|d| !(d.name == name && d.owner.as_deref() == owner.as_deref()));
    updated.push(new_deck);
    check_deck_slug_collisions(&updated, &owned)?;
    persist_custom_decks(&config_path, &entries_from(&updated))?;
    *guard = updated;

    Ok(if replaced {
        format!("Saved deck `{name}` updated.")
    } else {
        format!("Saved deck `{name}` created.")
    })
}

pub(super) fn delete_saved_deck_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    name: &str,
) -> Fallible<String> {
    let owner = owner_key(user);
    let Some(config_path) = state.config_path.lock().clone() else {
        return fail("Saved decks cannot be stored: no config file is in use.");
    };
    let mut guard = state.custom_decks.lock();
    // Matched on (name, owner): deleting your own deck must never remove
    // another user's deck of the same name.
    let is_target =
        |d: &ResolvedCustomDeck| d.name == name && d.owner.as_deref() == owner.as_deref();
    if !guard.iter().any(is_target) {
        return fail(format!("You have no saved deck called `{name}`."));
    }
    let mut updated = guard.clone();
    updated.retain(|d| !is_target(d));
    persist_custom_decks(&config_path, &entries_from(&updated))?;
    *guard = updated;
    Ok(format!(
        "Saved deck `{name}` deleted. Its cards and their review history are untouched."
    ))
}

// ── Tools ────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct SetSavedDeckArgs {
    /// The deck's name.
    pub name: String,
    /// The decks it draws on, each written `{collection-slug}/{deck-name}`,
    /// e.g. `Spanish/verbs.md`.
    pub members: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SavedDeckNameArgs {
    /// The deck's name.
    pub name: String,
}

#[tool_router(router = saved_router, vis = "pub(crate)")]
impl HashcardsMcp {
    #[tool(
        description = "List the user's saved decks. A saved deck is a named selection of decks \
                       from any of their collections, drilled together in one session; it holds \
                       no cards of its own."
    )]
    async fn list_saved_decks(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<Vec<SavedDeck>>, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || list_saved_decks_for(&state, caller.current_user().as_ref()))
            .await
            .map(Json)
            .map_err(to_mcp)
    }

    #[tool(description = "Create or replace a saved deck. Members are written \
                       `{collection-slug}/{deck-name}`, e.g. `Spanish/verbs.md`. This moves and \
                       copies nothing: the cards stay in their collections and keep their own \
                       schedules.")]
    async fn set_saved_deck(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<SetSavedDeckArgs>,
    ) -> Result<String, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            set_saved_deck_for(
                &state,
                caller.current_user().as_ref(),
                &args.name,
                &args.members,
            )
        })
        .await
        .map_err(to_mcp)
    }

    #[tool(
        description = "Delete a saved deck. The collections it drew on, and every card's review \
                       history, are untouched."
    )]
    async fn delete_saved_deck(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<SavedDeckNameArgs>,
    ) -> Result<String, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            delete_saved_deck_for(&state, caller.current_user().as_ref(), &args.name)
        })
        .await
        .map_err(to_mcp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::serve::mcp::tools::tests::mcp_fixture_with_config;
    use crate::cmd::serve::mcp::tools::tests::other_users_collection;

    #[test]
    fn a_saved_deck_round_trips_through_the_config_file() -> Fallible<()> {
        let (_dir, mcp, config_path) = mcp_fixture_with_config()?;
        set_saved_deck_for(
            &mcp.state,
            None,
            "Everything",
            &["Spanish/verbs.md".to_string()],
        )?;
        let toml = std::fs::read_to_string(&config_path)?;
        assert!(toml.contains("Everything"), "{toml}");

        let decks = list_saved_decks_for(&mcp.state, None)?;
        assert_eq!(decks.len(), 1);
        assert_eq!(decks[0].members, vec!["Spanish/verbs.md".to_string()]);
        Ok(())
    }

    /// The running server and its config file must not disagree: the swap
    /// happens under the same lock as the write, after it succeeds.
    #[test]
    fn a_saved_deck_is_live_without_a_restart() -> Fallible<()> {
        let (_dir, mcp, _config_path) = mcp_fixture_with_config()?;
        set_saved_deck_for(
            &mcp.state,
            None,
            "Everything",
            &["Spanish/verbs.md".to_string()],
        )?;
        assert!(
            mcp.state
                .custom_decks
                .lock()
                .iter()
                .any(|d| d.name == "Everything"),
            "the deck is in the file but not in the running server"
        );
        Ok(())
    }

    #[test]
    fn setting_the_same_deck_again_replaces_it() -> Fallible<()> {
        let (_dir, mcp, _config_path) = mcp_fixture_with_config()?;
        set_saved_deck_for(&mcp.state, None, "Exam", &["Spanish/verbs.md".to_string()])?;
        let msg = set_saved_deck_for(&mcp.state, None, "Exam", &["Spanish/verbs.md".to_string()])?;
        assert!(msg.contains("updated"), "{msg}");
        assert_eq!(list_saved_decks_for(&mcp.state, None)?.len(), 1);
        Ok(())
    }

    /// A saved deck must not become a way to read somebody else's cards.
    #[test]
    fn a_deck_naming_a_collection_that_is_not_yours_is_refused() -> Fallible<()> {
        let (dir, mcp, _config_path) = mcp_fixture_with_config()?;
        let theirs = other_users_collection(&dir)?;
        let err = set_saved_deck_for(&mcp.state, None, "Both", &[format!("{theirs}/nouns.md")])
            .unwrap_err();
        assert!(err.message().contains(&theirs), "{}", err.message());
        assert!(list_saved_decks_for(&mcp.state, None)?.is_empty());
        Ok(())
    }

    #[test]
    fn a_malformed_member_is_refused() -> Fallible<()> {
        let (_dir, mcp, _config_path) = mcp_fixture_with_config()?;
        let err =
            set_saved_deck_for(&mcp.state, None, "Bad", &["Spanish".to_string()]).unwrap_err();
        assert!(
            err.message().contains("collection-slug"),
            "{}",
            err.message()
        );
        Ok(())
    }

    #[test]
    fn a_deck_with_no_members_or_no_name_is_refused() -> Fallible<()> {
        let (_dir, mcp, _config_path) = mcp_fixture_with_config()?;
        assert!(set_saved_deck_for(&mcp.state, None, "Empty", &[]).is_err());
        assert!(
            set_saved_deck_for(&mcp.state, None, "   ", &["Spanish/verbs.md".to_string()]).is_err()
        );
        Ok(())
    }

    #[test]
    fn a_deleted_saved_deck_leaves_the_file_and_the_server() -> Fallible<()> {
        let (_dir, mcp, config_path) = mcp_fixture_with_config()?;
        set_saved_deck_for(
            &mcp.state,
            None,
            "Everything",
            &["Spanish/verbs.md".to_string()],
        )?;
        delete_saved_deck_for(&mcp.state, None, "Everything")?;
        assert!(!std::fs::read_to_string(&config_path)?.contains("Everything"));
        assert!(mcp.state.custom_decks.lock().is_empty());
        Ok(())
    }

    #[test]
    fn deleting_a_deck_that_is_not_there_is_refused() -> Fallible<()> {
        let (_dir, mcp, _config_path) = mcp_fixture_with_config()?;
        assert!(delete_saved_deck_for(&mcp.state, None, "Nothing").is_err());
        Ok(())
    }
}
