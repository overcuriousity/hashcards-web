//! The tool surface.
//!
//! Every tool is an adapter: it resolves the caller, hands the work to
//! `run_blocking`, and calls the same domain function the web handler
//! calls. Nothing here reimplements a rule -- which is why every guard the
//! browser gets (`refuse_if_drilling`, the slug-collision check, the
//! migration gate, `CardRoot`'s path checking) applies to a model too.

pub mod read;

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use tempfile::TempDir;

    use crate::auth_db::AuthDatabase;
    use crate::cmd::serve::cards::CardRoot;
    use crate::cmd::serve::cards::collection_id;
    use crate::cmd::serve::mcp::server::HashcardsMcp;
    use crate::cmd::serve::state::test_support::state_with_data_dir;
    use crate::error::Fallible;
    use crate::utils::ensure_dir;

    /// A server over one collection, `Spanish`, holding two cards.
    pub(crate) fn mcp_fixture() -> Fallible<(TempDir, HashcardsMcp)> {
        let dir = TempDir::new()?;
        let mut state = state_with_data_dir(dir.path().to_path_buf());
        state.auth = Some(Arc::new(AuthDatabase::open(&dir.path().join("auth.db"))?));
        let root = CardRoot::for_user(dir.path(), None)?;
        std::fs::create_dir_all(root.path().join("Spanish"))?;
        std::fs::write(
            root.path().join("Spanish/verbs.md"),
            "Q: hablar\nA: to speak\n\n---\n\nQ: comer\nA: to eat\n",
        )?;
        collection_id(&root.path().join("Spanish"))?;
        ensure_dir(&dir.path().join("db"), "review database directory")?;
        Ok((dir, HashcardsMcp::new(state)))
    }

    /// A second user's collection, for the isolation tests. Returns its
    /// slug, which the caller must not be able to reach.
    pub(crate) fn other_users_collection(dir: &TempDir) -> Fallible<String> {
        let theirs = CardRoot::for_user(dir.path(), Some("you@example.com"))?;
        std::fs::create_dir_all(theirs.path().join("German"))?;
        std::fs::write(
            theirs.path().join("German/nouns.md"),
            "Q: der Hund\nA: the dog\n",
        )?;
        collection_id(&theirs.path().join("German"))?;
        Ok("German".to_string())
    }
}
