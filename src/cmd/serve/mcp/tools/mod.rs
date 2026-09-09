//! The tool surface.
//!
//! Every tool is an adapter: it resolves the caller, hands the work to
//! `run_blocking`, and calls the same domain function the web handler
//! calls. Nothing here reimplements a rule -- which is why every guard the
//! browser gets (`refuse_if_drilling`, the slug-collision check, the
//! migration gate, `CardRoot`'s path checking) applies to a model too.

pub mod cards;
pub mod collections;
pub mod decks;
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

    /// A drill session on `slug`, holding `cards` in its queue — the shape
    /// a real drill leaves in the sessions map.
    pub(crate) fn seed_session(
        state: &crate::cmd::serve::state::AppState,
        data_dir: &std::path::Path,
        folder: &std::path::Path,
        slug: &str,
        cards: Vec<crate::types::card::Card>,
    ) -> Fallible<()> {
        use crate::cmd::drill::cache::Cache;
        use crate::cmd::drill::render::AnswerControls;
        use crate::cmd::drill::state::MutableState;
        use crate::cmd::drill::state::SessionDbs;
        use crate::cmd::serve::cards::user_db_path;
        use crate::cmd::serve::state::DrillSession;
        use crate::cmd::serve::state::SessionKey;
        use crate::rng::TinyRng;
        use crate::types::performance::Jitter;
        use crate::types::performance::Scheduling;
        use crate::types::timestamp::Timestamp;
        use crate::user_db::UserDatabase;

        let root = CardRoot::for_user(data_dir, None)?;
        let db_dir = data_dir.join("db");
        ensure_dir(&db_dir, "review database directory")?;
        let id = collection_id(folder)?;
        let db = UserDatabase::open(&user_db_path(&root, &db_dir)?)?.collection(id);
        let started_at = Timestamp::now();
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
            Cache::new(),
            cards,
            TinyRng::from_seed(1),
        );
        let session = Arc::new(parking_lot::Mutex::new(DrillSession::new(
            folder.to_path_buf(),
            Vec::new(),
            started_at,
            AnswerControls::Full,
            mutable,
        )));
        state
            .sessions
            .lock()
            .insert(SessionKey::new(None, slug), session);
        Ok(())
    }

    /// The card hashes a session is still holding in its queue.
    pub(crate) fn session_card_hashes(
        state: &crate::cmd::serve::state::AppState,
        slug: &str,
    ) -> Vec<String> {
        use crate::cmd::serve::state::SessionKey;
        let sessions = state.sessions.lock();
        match sessions.get(&SessionKey::new(None, slug)) {
            Some(s) => s
                .lock()
                .mutable
                .cards
                .iter()
                .map(|c| c.hash().to_string())
                .collect(),
            None => Vec::new(),
        }
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
