mod auth;
mod bookmarks;
mod browse;
mod cards;
pub mod config;
mod counts;
mod decks;
mod edit;
pub mod export;
mod files;
mod files_ui;
mod handlers;
mod href;
mod landing;
mod merge;
mod reviewdb;
pub mod server;
mod state;
mod tokens;
mod trash;
mod trash_ui;
mod upload;

pub mod stats;

#[cfg(test)]
mod tests {
    use std::fs::write;
    use std::path::PathBuf;

    use portpicker::pick_unused_port;
    use tempfile::TempDir;
    use tempfile::tempdir;
    use tokio::spawn;

    use crate::cmd::drill::fonts::FONT_DIR_URL;
    use crate::cmd::drill::hljs::HLJS_CSS_URL;
    use crate::cmd::drill::hljs::HLJS_JS_URL;
    use crate::cmd::drill::katex::KATEX_CSS_URL;
    use crate::cmd::drill::katex::KATEX_JS_URL;
    use crate::cmd::drill::katex::KATEX_MHCHEM_JS_URL;
    use crate::cmd::drill::template::STYLE_URL;
    use crate::cmd::serve::config::DefaultsSection;
    use crate::cmd::serve::config::ResolvedMcp;
    use crate::cmd::serve::config::ResolvedServeConfig;
    use crate::cmd::serve::server::start_serve;
    use crate::error::ErrorReport;
    use crate::error::Fallible;
    use crate::error::fail;
    use crate::types::card_hash::CardHash;
    use crate::types::collection_id::CollectionId;
    use crate::types::performance::Performance;
    use crate::types::timestamp::Timestamp;
    use crate::user_db::UserDatabase;
    use crate::utils::CACHE_CONTROL_IMMUTABLE;
    use crate::utils::CACHE_CONTROL_REVALIDATE;
    use crate::utils::wait_for_server;

    const TEST_HOST: &str = "127.0.0.1";

    /// Create a collection named `name` in the default card tree under
    /// `data_dir`, holding `files`, and stamp it with a stable id so the
    /// read paths can find it.
    ///
    /// The folder name *is* the URL slug: discovery slugifies it, so a name
    /// with a space becomes a slug with a dash.
    ///
    /// Returns the collection folder, its owner's review database path, and
    /// the id that scopes this collection's rows inside it.
    fn card_collection(
        data_dir: &std::path::Path,
        name: &str,
        files: &[(&str, &str)],
    ) -> Fallible<(PathBuf, PathBuf, CollectionId)> {
        use crate::cmd::serve::cards::CardRoot;
        use crate::cmd::serve::cards::collection_id;
        use crate::cmd::serve::cards::user_db_path;

        let root = CardRoot::for_user(data_dir, None)?;
        let folder = root.path().join(name);
        std::fs::create_dir_all(&folder)?;
        for (rel, content) in files {
            let path = folder.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            write(path, content)?;
        }
        let id = collection_id(&folder)?;
        let db_dir = data_dir.join("db");
        std::fs::create_dir_all(&db_dir)?;
        Ok((folder, user_db_path(&root, &db_dir)?, id))
    }

    /// Serve `data_dir` on `port`, and wait until it answers.
    async fn serve_data_dir(data_dir: &std::path::Path, port: u16) -> Fallible<()> {
        let config = ResolvedServeConfig {
            host: TEST_HOST.to_string(),
            port,
            defaults: DefaultsSection::default(),
            data_dir: Some(data_dir.to_path_buf()),
            config_path: None,
            custom_decks: Vec::new(),
            session_timeout_minutes: 1440,
            mcp: ResolvedMcp::default(),
            oidc: None,
        };
        spawn(async move { start_serve(config).await });
        wait_for_server(TEST_HOST, port).await
    }

    /// The hash of the card the drill page is showing, out of its hidden
    /// `card` input.
    fn extract_card_hash(html: &str) -> Fallible<String> {
        extract_input_value(html, "card")
    }

    /// The `value` of the first `<input name="{name}" … value="…">` on the
    /// page. Deliberately crude: it exists so a test can act as a browser
    /// would, not to parse HTML in general.
    fn extract_input_value(html: &str, name: &str) -> Fallible<String> {
        let needle = format!("name=\"{name}\"");
        let after = match html.split_once(&needle) {
            Some((_, rest)) => rest,
            None => return fail(format!("no input named `{name}` on the page")),
        };
        let after = match after.split_once("value=\"") {
            Some((_, rest)) => rest,
            None => return fail(format!("input `{name}` has no value")),
        };
        match after.split_once('"') {
            Some((value, _)) => Ok(value.to_string()),
            None => fail(format!("input `{name}` has an unterminated value")),
        }
    }

    /// A server whose card tree holds one collection named `name`. Returns
    /// the port and the temp directory, which the caller must keep alive.
    async fn spawn_test_server(name: &str, files: &[(&str, &str)]) -> Fallible<(u16, TempDir)> {
        let port = pick_unused_port().unwrap();
        let dir = tempdir()?;
        card_collection(dir.path(), name, files)?;
        serve_data_dir(dir.path(), port).await?;
        Ok((port, dir))
    }

    #[tokio::test]
    async fn test_flash_query_param_renders_on_collection_page() -> Fallible<()> {
        let slug = "test-collection";
        let (port, _dir) = spawn_test_server(
            slug,
            &[(
                "Alpha.md",
                "Q: What is 1+1?
A: 2
",
            )],
        )
        .await?;

        let response = reqwest::get(format!(
            "http://{TEST_HOST}:{port}/collection/{slug}?flash=Hello%20world&kind=success"
        ))
        .await?;
        let body = response.text().await?;
        assert!(body.contains("flash-success"), "body: {body}");
        assert!(body.contains("Hello world"));
        Ok(())
    }

    /// The theme switch is three pieces that have to agree: the stylesheet's
    /// two override selectors, the inline script that applies a stored choice
    /// before the first paint, and the button `script.js` looks for. Any one
    /// of them renamed alone leaves a switch that does nothing.
    #[tokio::test]
    async fn test_theme_switch_is_wired_end_to_end() -> Fallible<()> {
        let slug = "test-collection";
        let (port, _dir) = spawn_test_server(
            slug,
            &[(
                "Alpha.md",
                "Q: What is 1+1?
A: 2
",
            )],
        )
        .await?;

        let css = reqwest::get(format!("http://{TEST_HOST}:{port}/style.css"))
            .await?
            .text()
            .await?;
        assert!(
            css.contains(r#":root[data-theme="dark"]"#),
            "no dark override"
        );
        assert!(
            css.contains(r#":root:not([data-theme="light"])"#),
            "no light opt-out"
        );

        let page = reqwest::get(format!("http://{TEST_HOST}:{port}/"))
            .await?
            .text()
            .await?;
        assert!(page.contains("hashcards.theme"), "no pre-paint script");
        assert!(page.contains("data-theme-toggle"), "no toggle button");
        assert!(page.contains("data-theme-label"), "no toggle label");

        let js = reqwest::get(format!("http://{TEST_HOST}:{port}/script.js"))
            .await?
            .text()
            .await?;
        assert!(
            js.contains("data-theme-toggle"),
            "script.js does not bind the toggle"
        );
        assert!(
            js.contains("hashcards.theme"),
            "script.js stores under another key"
        );
        Ok(())
    }

    /// The stylesheet names four font files and serves them from the binary.
    /// A typo in either the route or a filename is invisible in the browser —
    /// the page simply falls back to a system font — so the names are checked
    /// here instead.
    #[tokio::test]
    async fn test_fonts_are_served() -> Fallible<()> {
        let slug = "test-collection";
        let (port, _dir) = spawn_test_server(
            slug,
            &[(
                "Alpha.md",
                "Q: What is 1+1?
A: 2
",
            )],
        )
        .await?;

        let dir = FONT_DIR_URL.as_str();
        let css = reqwest::get(format!("http://{TEST_HOST}:{port}{}", STYLE_URL.as_str()))
            .await?
            .text()
            .await?;

        for name in [
            "inter-400.woff2",
            "inter-500.woff2",
            "inter-600.woff2",
            "jetbrains-mono-400.woff2",
        ] {
            assert!(
                css.contains(&format!("{dir}/{name}")),
                "{name} not in style.css"
            );
            let response = reqwest::get(format!("http://{TEST_HOST}:{port}{dir}/{name}")).await?;
            assert!(
                response.status().is_success(),
                "{name}: {:?}",
                response.status()
            );
            assert_eq!(
                response
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok()),
                Some("font/woff2"),
                "{name}"
            );
            assert!(!response.bytes().await?.is_empty(), "{name} is empty");
        }

        // A name that is not one of the four is a 404, not a path to read.
        let response =
            reqwest::get(format!("http://{TEST_HOST}:{port}{dir}/..%2Fstyle.css")).await?;
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        Ok(())
    }

    /// Every asset served `immutable` is served from a path that names this
    /// build's copy of it, and every one of those paths answers a revision it
    /// does not recognise with the current bytes.
    ///
    /// Regression: `immutable` forbids revalidation, so an asset at a fixed
    /// path is frozen in a client's cache for a week after it changes. The
    /// first half of the fix — a revisioned path — is only half of it: HTML
    /// a client already holds still asks for the *old* revision, and a 404
    /// there is a page with no stylesheet, no maths and no highlighting,
    /// which is worse than the staleness being fixed.
    #[tokio::test]
    async fn test_revisioned_assets_answer_a_stale_revision() -> Fallible<()> {
        let slug = "test-collection";
        let (port, _dir) =
            spawn_test_server(slug, &[("Alpha.md", "Q: What is 1+1?\nA: 2\n")]).await?;

        let font = format!("{}/inter-400.woff2", FONT_DIR_URL.as_str());
        let katex_font = format!(
            "/katex/fonts/{}/KaTeX_Main-Regular.woff2",
            crate::cmd::drill::katex::KATEX_REV.as_str()
        );
        let current = [
            STYLE_URL.as_str(),
            KATEX_CSS_URL.as_str(),
            KATEX_JS_URL.as_str(),
            KATEX_MHCHEM_JS_URL.as_str(),
            HLJS_CSS_URL.as_str(),
            HLJS_JS_URL.as_str(),
            font.as_str(),
            katex_font.as_str(),
        ];

        for url in current {
            let response = reqwest::get(format!("http://{TEST_HOST}:{port}{url}")).await?;
            assert!(
                response.status().is_success(),
                "{url}: {:?}",
                response.status()
            );
            assert_eq!(
                response
                    .headers()
                    .get("cache-control")
                    .and_then(|v| v.to_str().ok()),
                Some(CACHE_CONTROL_IMMUTABLE),
                "{url} is not cacheable, so the revision buys nothing"
            );

            // The same asset as an older build's HTML asks for it.
            let stale = stale_revision(url);
            let response = reqwest::get(format!("http://{TEST_HOST}:{port}{stale}")).await?;
            assert!(
                response.status().is_success(),
                "{stale} 404s, so a client with cached HTML loses the asset entirely"
            );
            assert_eq!(
                response
                    .headers()
                    .get("cache-control")
                    .and_then(|v| v.to_str().ok()),
                Some(CACHE_CONTROL_REVALIDATE),
                "{stale} must not be kept: the path does not name what was served"
            );
            assert!(!response.bytes().await?.is_empty(), "{stale} is empty");
        }
        Ok(())
    }

    /// The same URL with a revision no build ever produced.
    fn stale_revision(url: &str) -> String {
        let mut parts: Vec<&str> = url.split('/').collect();
        let rev = parts
            .iter()
            .position(|p| p.len() == 16 && p.chars().all(|c| c.is_ascii_hexdigit()))
            .expect("every revisioned URL names a revision");
        parts[rev] = "00000000deadbeef";
        parts.join("/")
    }

    /// The fixed paths every build before revisioning rendered into its HTML.
    /// A page from such a build is still in caches, and must still find its
    /// assets — served so that it cannot keep them.
    #[tokio::test]
    async fn test_legacy_asset_paths_still_serve() -> Fallible<()> {
        let slug = "test-collection";
        let (port, _dir) =
            spawn_test_server(slug, &[("Alpha.md", "Q: What is 1+1?\nA: 2\n")]).await?;

        for url in [
            "/style.css",
            "/fonts/inter-400.woff2",
            "/katex/katex.css",
            "/katex/katex.js",
            "/katex/mhchem.js",
            "/katex/fonts/KaTeX_Main-Regular.woff2",
            "/hljs/github.css",
            "/hljs/highlight.js",
        ] {
            let response = reqwest::get(format!("http://{TEST_HOST}:{port}{url}")).await?;
            assert!(
                response.status().is_success(),
                "{url}: {:?}",
                response.status()
            );
            assert_eq!(
                response
                    .headers()
                    .get("cache-control")
                    .and_then(|v| v.to_str().ok()),
                Some(CACHE_CONTROL_REVALIDATE),
                "{url} does not name its contents and must not be cached blind"
            );
            assert!(!response.bytes().await?.is_empty(), "{url} is empty");
        }
        Ok(())
    }

    /// Regression: with no [oidc] configured, /auth/login must not exist —
    /// proves the auth routes and middleware are opt-in, not always-on.
    #[tokio::test]
    async fn test_auth_routes_absent_without_oidc_config() -> Fallible<()> {
        let slug = "test-collection";
        let (port, _dir) = spawn_test_server(
            slug,
            &[(
                "Alpha.md",
                "Q: What is 1+1?
A: 2
",
            )],
        )
        .await?;

        let response = reqwest::Client::new()
            .get(format!("http://{TEST_HOST}:{port}/auth/login"))
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);

        // And the collection page itself is still reachable with no login.
        let response = reqwest::get(format!("http://{TEST_HOST}:{port}/collection/{slug}")).await?;
        assert!(response.status().is_success());
        Ok(())
    }

    /// A collection created through the web interface must be drillable:
    /// discovered, listed on the landing page, and reachable by slug.
    #[tokio::test]
    async fn test_a_locally_created_collection_can_be_drilled() -> Fallible<()> {
        let port = pick_unused_port().unwrap();
        let dir = tempdir()?;
        let data_dir = dir.path().to_path_buf();

        serve_data_dir(&data_dir, port).await?;

        let client = reqwest::Client::new();
        let base = format!("http://{TEST_HOST}:{port}");

        client
            .post(format!("{base}/files/folder"))
            .form(&[("parent", ""), ("name", "Spanish")])
            .send()
            .await?;
        client
            .post(format!("{base}/files/file"))
            .form(&[("parent", "Spanish"), ("name", "verbs")])
            .send()
            .await?;

        // The landing page must offer it, or nothing can be drilled.
        let landing = client.get(&base).send().await?.text().await?;
        assert!(
            landing.contains("/collection/Spanish"),
            "landing page did not list the local collection, got: {landing}"
        );

        // And it must be reachable by slug.
        let page = client
            .get(format!("{base}/collection/Spanish"))
            .send()
            .await?;
        assert!(
            page.status().is_success(),
            "collection page returned {}",
            page.status()
        );
        Ok(())
    }
    /// Regression test: POSTing multiple `decks` values to /collection/{slug}/start
    /// must not fail with "duplicate field `decks`".
    #[tokio::test]
    async fn test_start_with_multiple_decks() -> Fallible<()> {
        let slug = "test-collection".to_string();
        // Two markdown files representing two different topics.
        let (port, _dir) = spawn_test_server(
            &slug,
            &[
                ("Alpha.md", "Q: What is 1+1?\nA: 2\n"),
                ("Beta.md", "Q: What is 2+2?\nA: 4\n"),
            ],
        )
        .await?;

        // POST with multiple `decks` values — this used to fail with
        // "Failed to deserialize form body: duplicate field `decks`".
        let response = reqwest::Client::new()
            .post(format!("http://{TEST_HOST}:{port}/collection/{slug}/start"))
            .body("decks=Alpha&decks=Beta")
            .header("content-type", "application/x-www-form-urlencoded")
            .send()
            .await?;

        // The handler redirects on success; reqwest follows redirects by
        // default, so any 2xx status means the form was accepted.
        assert!(
            response.status().is_success(),
            "expected success, got {}",
            response.status()
        );

        // The redirect target must show the running drill session, not the
        // deck browser (the redirect alone fires on success and failure).
        let body = response.text().await?;
        assert!(
            body.contains("value=\"Reveal\""),
            "expected the post-redirect page to show the drill session, got: {body}"
        );

        Ok(())
    }

    /// The collection list starts a session in one tap. It used to lead to a
    /// topic picker whose own button also said "Drill", so studying always
    /// cost two presses of the same word.
    #[tokio::test]
    async fn test_drill_from_the_list_lands_on_a_card() -> Fallible<()> {
        let slug = "test-collection";
        let (port, _dir) = spawn_test_server(
            slug,
            &[
                ("Alpha.md", "Q: What is 1+1?\nA: 2\n"),
                ("Beta.md", "Q: What is 2+2?\nA: 4\n"),
            ],
        )
        .await?;
        let base = format!("http://{TEST_HOST}:{port}");
        let client = reqwest::Client::new();

        // The list's Drill button carries no topic checkboxes at all.
        let list = client.get(format!("{base}/")).send().await?.text().await?;
        assert!(
            list.contains(r#"name="all_topics""#),
            "the list must post the one-tap start: {list}"
        );

        let body = client
            .post(format!("{base}/collection/{slug}/start"))
            .body("all_topics=1")
            .header("content-type", "application/x-www-form-urlencoded")
            .send()
            .await?
            .text()
            .await?;
        // Card one, not the topic picker.
        assert!(
            body.contains("value=\"Reveal\""),
            "one tap must land on a card: {body}"
        );
        assert!(
            !body.contains("deck-tree"),
            "the topic picker must not stand in the way: {body}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_start_with_no_decks_is_rejected_with_flash() -> Fallible<()> {
        let slug = "test-collection";
        let (port, _dir) = spawn_test_server(
            slug,
            &[(
                "Alpha.md",
                "Q: What is 1+1?
A: 2
",
            )],
        )
        .await?;

        // POST with no `decks` field at all (no-JS or hand-made form).
        let response = reqwest::Client::new()
            .post(format!("http://{TEST_HOST}:{port}/collection/{slug}/start"))
            .body("")
            .header("content-type", "application/x-www-form-urlencoded")
            .send()
            .await?;
        let body = response.text().await?;
        // The post-redirect page shows the flash and stays on the deck browser:
        assert!(body.contains("Select at least one topic"), "body: {body}");
        assert!(body.contains("flash-error"));
        // No session was started (a session page would show the Reveal button).
        assert!(!body.contains("value=\"Reveal\""));
        Ok(())
    }

    #[tokio::test]
    async fn test_bookmark_delete_error_is_surfaced_as_flash() -> Fallible<()> {
        let slug = "test-collection";
        let (port, _dir) = spawn_test_server(
            slug,
            &[(
                "Alpha.md",
                "Q: What is 1+1?
A: 2
",
            )],
        )
        .await?;

        // "nothex" is not a valid card hash: the delete must fail, and the
        // failure must be visible on the post-redirect bookmarks page.
        let response = reqwest::Client::new()
            .post(format!(
                "http://{TEST_HOST}:{port}/collection/{slug}/bookmarks/nothex/delete"
            ))
            .send()
            .await?;
        let body = response.text().await?;
        assert!(body.contains("flash-error"), "body: {body}");
        Ok(())
    }

    #[tokio::test]
    async fn test_landing_counts_refresh_after_session_finish() -> Fallible<()> {
        let slug = "count-collection".to_string();
        let (port, _dir) =
            spawn_test_server(&slug, &[("Deck.md", "Q: What is 1+1?\nA: 2\n")]).await?;

        let base = format!("http://{TEST_HOST}:{port}");
        let client = reqwest::Client::new();

        // Sanity: the one new card is due, so the landing page offers a drill.
        let body = client.get(format!("{base}/")).send().await?.text().await?;
        assert!(
            body.contains("Drill"),
            "expected a due card before the session: {body}"
        );

        // Start and complete the one-card session.
        client
            .post(format!("{base}/collection/{slug}/start"))
            .body("decks=Deck")
            .header("content-type", "application/x-www-form-urlencoded")
            .send()
            .await?;
        for action in ["Reveal", "Good"] {
            client
                .post(format!("{base}/collection/{slug}"))
                .body(format!("action={action}"))
                .header("content-type", "application/x-www-form-urlencoded")
                .send()
                .await?;
        }

        // The refresh runs in the background; poll the landing page briefly.
        let mut refreshed = false;
        for _ in 0..40 {
            let body = client.get(format!("{base}/")).send().await?.text().await?;
            if body.contains("Nothing due") {
                refreshed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        assert!(
            refreshed,
            "landing page still shows stale due counts after the session finished"
        );
        Ok(())
    }

    /// FEAT-03: an unfinished session is offered for resumption on the
    /// landing page and is NOT silently discarded by another start POST.
    #[tokio::test]
    async fn test_unfinished_session_is_offered_for_resume_not_replaced() -> Fallible<()> {
        let port = pick_unused_port().unwrap();
        let dir = tempdir()?;
        let slug = "resume-collection".to_string();
        let (_folder, db_path, id) = card_collection(
            dir.path(),
            &slug,
            &[(
                "Deck.md",
                "Q: What is 1+1?\nA: 2\n\n---\n\nQ: What is 2+2?\nA: 4\n",
            )],
        )?;
        serve_data_dir(dir.path(), port).await?;

        let base = format!("http://{TEST_HOST}:{port}");
        let client = reqwest::Client::new();
        let start = || {
            client
                .post(format!("{base}/collection/{slug}/start"))
                .body("decks=Deck")
                .header("content-type", "application/x-www-form-urlencoded")
                .send()
        };
        start().await?;

        // The landing page offers to resume the running two-card session.
        let body = client.get(format!("{base}/")).send().await?.text().await?;
        assert!(
            body.contains("Resume (2 left)"),
            "landing page must offer resume: {body}"
        );

        // A second start POST must not discard the session: still one DB row.
        start().await?;
        let db = UserDatabase::open(&db_path)?.collection(id);
        assert_eq!(
            db.get_all_sessions()?.len(),
            1,
            "second start POST must not create a new session"
        );
        Ok(())
    }

    /// A server started on a data directory seeded with pre-consolidation
    /// databases serves the review history they hold. This is the upgrade,
    /// end to end.
    #[tokio::test]
    async fn test_legacy_databases_are_merged_at_startup() -> Fallible<()> {
        let port = pick_unused_port().unwrap();
        let dir = tempdir()?;
        let slug = "legacy-collection".to_string();
        let (folder, _db_path, id) =
            card_collection(dir.path(), &slug, &[("Deck.md", "Q: What is 1+1?\nA: 2\n")])?;

        // The card, hashed exactly as the old database would have had it.
        let parsed = crate::parser::parse_deck(&folder)?;
        let hash = parsed.cards[0].hash().to_hex();

        // A pre-consolidation database for that collection, with one review
        // and a due date far in the future.
        {
            let legacy = crate::db::test_support::create_legacy_v7(
                &dir.path().join("db").join(format!("{id}.db")),
            )?;
            legacy.execute(
                "insert into cards (card_hash, added_at, due_date, review_count) \
                 values (?, '2026-01-01T09:00:00.000', '2099-01-01', 1);",
                rusqlite::params![hash],
            )?;
            legacy.execute(
                "insert into sessions (session_id, started_at, ended_at) \
                 values (1, '2026-01-01T09:00:00.000', '2026-01-01T09:30:00.000');",
                [],
            )?;
            legacy.execute(
                "insert into reviews (session_id, card_hash, reviewed_at, grade, stability, \
                 difficulty, interval_raw, interval_days, due_date) values \
                 (1, ?, '2026-01-01T09:00:00.000', 'good', 2.0, 5.0, 2.0, 2, '2099-01-01');",
                rusqlite::params![hash],
            )?;
        }

        serve_data_dir(dir.path(), port).await?;

        // The card is scheduled far in the future, so the collection page
        // must report nothing due — which it can only know from the merged
        // history. Without the merge it would be a brand-new card, and due.
        let body = reqwest::get(format!("http://{TEST_HOST}:{port}/collection/{slug}"))
            .await?
            .text()
            .await?;
        assert!(
            !body.contains("1 due"),
            "the merged review history must be in force: {body}"
        );
        // And the source is kept.
        assert!(
            dir.path()
                .join("db")
                .join("legacy")
                .join(format!("{id}.db"))
                .exists(),
            "the source database must be kept, not deleted"
        );
        Ok(())
    }

    /// FEAT-03: a dangling DB session row (left by a crash/restart) is closed
    /// and reported on the deck browser page. It cannot be rehydrated: the
    /// card queue only exists in memory.
    #[tokio::test]
    async fn test_dangling_session_row_is_closed_and_reported() -> Fallible<()> {
        let port = pick_unused_port().unwrap();
        let dir = tempdir()?;
        let slug = "dangling-collection".to_string();
        let (_folder, db_path, id) =
            card_collection(dir.path(), &slug, &[("Deck.md", "Q: What is 1+1?\nA: 2\n")])?;

        // Simulate a crash: a session row that was never closed.
        {
            let db = UserDatabase::open(&db_path)?.collection(id);
            let t0 = Timestamp::try_from("2026-01-01T10:00:00.000".to_string())?;
            db.create_session(t0)?;
        }

        serve_data_dir(dir.path(), port).await?;

        let body = reqwest::get(format!("http://{TEST_HOST}:{port}/collection/{slug}"))
            .await?
            .text()
            .await?;
        assert!(
            body.contains("interrupted session"),
            "deck browser must report the closed interrupted session: {body}"
        );

        // The sweep runs once, at startup, so the notice is reported once.
        // Re-closing on every browse would also stamp `ended_at` on sessions
        // that are still live in a concurrent CLI `drill`.
        let second = reqwest::get(format!("http://{TEST_HOST}:{port}/collection/{slug}"))
            .await?
            .text()
            .await?;
        assert!(
            !second.contains("interrupted session"),
            "the notice must not reappear on every visit: {second}"
        );
        Ok(())
    }

    /// Regression test (BUG-01): a request error mid-session must not drop
    /// the drill session. Forces a render error by deleting the card's
    /// source file, then asserts the session survives and the next GET
    /// renders the same card rather than the deck browser.
    #[tokio::test]
    async fn test_session_survives_render_error() -> Fallible<()> {
        let slug = "test-collection".to_string();
        let (port, dir) =
            spawn_test_server(&slug, &[("Alpha.md", "Q: What is 1+1?\nA: 2\n")]).await?;
        let card_file = dir
            .path()
            .join("cards")
            .join("default")
            .join(&slug)
            .join("Alpha.md");
        let client = reqwest::Client::new();

        // Start a drill session; the redirect is followed to the session page.
        let response = client
            .post(format!("http://{TEST_HOST}:{port}/collection/{slug}/start"))
            .body("decks=Alpha")
            .header("content-type", "application/x-www-form-urlencoded")
            .send()
            .await?;
        assert!(response.status().is_success());
        let body = response.text().await?;
        assert!(
            body.contains("progress-bar"),
            "expected a running session, got: {body}"
        );

        // Force a render error mid-session: the card's source file vanishes.
        std::fs::remove_file(&card_file)?;
        let response = client
            .get(format!("http://{TEST_HOST}:{port}/collection/{slug}"))
            .send()
            .await?;
        assert_eq!(
            response.status(),
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        );

        // Restore the file (identical content, identical hash). The session
        // must have survived the error: the next GET renders the same card,
        // not the deck browser.
        write(&card_file, "Q: What is 1+1?\nA: 2\n")?;
        let response = client
            .get(format!("http://{TEST_HOST}:{port}/collection/{slug}"))
            .send()
            .await?;
        assert!(response.status().is_success());
        let body = response.text().await?;
        assert!(
            body.contains("progress-bar"),
            "session was dropped by the render error: {body}"
        );
        assert!(
            !body.contains("deck-tree"),
            "deck browser rendered instead of the surviving session"
        );
        Ok(())
    }

    /// A file name may hold `#` or `?`, which end a URL's path. The tree
    /// links to it and the editor posts back to it percent-encoded, and axum
    /// decodes the path parameter, so the round trip has to land on the file
    /// the user actually clicked.
    #[tokio::test]
    async fn test_a_file_named_with_a_url_delimiter_opens_in_the_editor() -> Fallible<()> {
        let dir = tempdir()?;
        let data_dir = dir.path().to_path_buf();
        let port = pick_unused_port().unwrap();
        serve_data_dir(&data_dir, port).await?;

        let folder = data_dir.join("cards").join("default").join("Spanish");
        std::fs::create_dir_all(&folder)?;
        write(folder.join("a#b.md"), "Q: the cat\nA: el gato\n")?;

        let tree = reqwest::get(format!("http://{TEST_HOST}:{port}/files"))
            .await?
            .text()
            .await?;
        assert!(
            tree.contains("/files/edit/Spanish/a%23b.md"),
            "tree: {tree}"
        );

        let page = reqwest::get(format!(
            "http://{TEST_HOST}:{port}/files/edit/Spanish/a%23b.md"
        ))
        .await?
        .text()
        .await?;
        assert!(page.contains("el gato"), "page: {page}");
        Ok(())
    }

    /// The whole paste path, end to end: the bytes go up, the markdown path
    /// comes back, a card that references it saves, the collection still
    /// loads, and the image itself is served back through the collection's
    /// file endpoint. Any one of those links breaking leaves the user with a
    /// card that shows a broken picture — or a collection that will not open.
    #[tokio::test]
    async fn test_a_pasted_image_is_stored_referenced_and_served() -> Fallible<()> {
        let dir = tempdir()?;
        let data_dir = dir.path().to_path_buf();
        let port = pick_unused_port().unwrap();
        serve_data_dir(&data_dir, port).await?;

        let deck_dir = data_dir.join("cards").join("default").join("Spanish");
        std::fs::create_dir_all(&deck_dir)?;
        write(deck_dir.join("verbs.md"), "Q: a\nA: b\n")?;

        // A one-pixel PNG, header and all: the endpoint reads the magic
        // number, so a fake body would be refused.
        let png: Vec<u8> = {
            let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
            v.extend_from_slice(b"the rest of a very small picture");
            v
        };
        let client = reqwest::Client::new();
        let base = format!("http://{TEST_HOST}:{port}");

        let response = client
            .post(format!("{base}/files/media/Spanish/verbs.md"))
            .header("content-type", "image/png")
            .body(png.clone())
            .send()
            .await?;
        assert_eq!(response.status(), 200);
        let inserted = response.text().await?;
        assert!(inserted.starts_with("@/media/"), "got: {inserted}");

        // Anything that is not an image is refused, and says why.
        let refused = client
            .post(format!("{base}/files/media/Spanish/verbs.md"))
            .body(b"%PDF-1.7\n".to_vec())
            .send()
            .await?;
        assert_eq!(refused.status(), 400);
        assert!(refused.text().await?.contains("PNG"), "no reason given");

        // A card referencing it saves, and the collection still loads: a
        // reference to a missing file fails `validate_media_files` and takes
        // the whole collection page down with it.
        let page = reqwest::get(format!("{base}/files/edit/Spanish/verbs.md"))
            .await?
            .text()
            .await?;
        let mtime = page
            .split("name=\"mtime\" value=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or_default()
            .to_string();
        let saved = client
            .post(format!("{base}/files/edit/Spanish/verbs.md"))
            .form(&[
                ("mtime", mtime.as_str()),
                ("content", &format!("Q: what is this\nA: ![]({inserted})\n")),
            ])
            .send()
            .await?;
        assert_eq!(saved.status(), 200, "the save redirect must land");

        let collection = reqwest::get(format!("{base}/collection/Spanish"))
            .await?
            .text()
            .await?;
        assert!(
            !collection.contains("Missing media"),
            "collection: {collection}"
        );

        // And the image comes back through the collection's own endpoint,
        // which is what the rendered card asks for.
        let name = inserted.trim_start_matches("@/");
        let served = reqwest::get(format!("{base}/collection/Spanish/file/{name}")).await?;
        assert_eq!(served.status(), 200);
        assert_eq!(
            served
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("image/png")
        );
        assert_eq!(served.bytes().await?.to_vec(), png);
        Ok(())
    }

    /// Editing the card you are drilling used to be refused outright. Now
    /// the session follows the card to its new hash, so the grade that
    /// comes next lands on the card that is actually on screen — and the
    /// old card's review history came with it.
    #[tokio::test]
    async fn test_a_card_can_be_edited_mid_session_and_then_graded() -> Fallible<()> {
        let (port, dir) =
            spawn_test_server("Spanish", &[("verbs.md", "Q: the cat\nA: el gato\n")]).await?;
        let base = format!("http://{TEST_HOST}:{port}");
        let client = reqwest::Client::new();

        // Start a drill on the whole collection.
        client
            .post(format!("{base}/collection/Spanish/start"))
            .form(&[("decks", "verbs")])
            .send()
            .await?;
        let page = client
            .get(format!("{base}/collection/Spanish"))
            .send()
            .await?
            .text()
            .await?;
        let old_hash = extract_card_hash(&page)?;

        // Reveal, then edit the card that is on screen.
        client
            .post(format!("{base}/collection/Spanish"))
            .form(&[("action", "Reveal"), ("card", old_hash.as_str())])
            .send()
            .await?;

        let form = client
            .get(format!("{base}/collection/Spanish/edit/{old_hash}"))
            .send()
            .await?
            .text()
            .await?;
        let mtime = extract_input_value(&form, "mtime_ms")?;
        let save = client
            .post(format!("{base}/collection/Spanish/edit/{old_hash}"))
            .form(&[
                ("new_text", "Q: the cat\nA: el gato (masc.)"),
                ("mtime_ms", mtime.as_str()),
            ])
            .send()
            .await?;
        assert!(
            save.status().is_success(),
            "save returned {}",
            save.status()
        );

        // The session is still live, and now shows the edited card.
        let page = client
            .get(format!("{base}/collection/Spanish"))
            .send()
            .await?
            .text()
            .await?;
        let new_hash = extract_card_hash(&page)?;
        assert_ne!(new_hash, old_hash, "the edit must have renamed the card");

        // Grading it must be accepted, not answered with "already graded".
        client
            .post(format!("{base}/collection/Spanish"))
            .form(&[("action", "Reveal"), ("card", new_hash.as_str())])
            .send()
            .await?;
        client
            .post(format!("{base}/collection/Spanish"))
            .form(&[("action", "Good"), ("card", new_hash.as_str())])
            .send()
            .await?;

        // The review landed on the new hash, in the collection's own
        // database, with the old card's row carried over.
        let folder = dir.path().join("cards").join("default").join("Spanish");
        let id = crate::cmd::serve::cards::existing_collection_id(&folder)?
            .ok_or_else(|| ErrorReport::new("the collection has no id"))?;
        let root = crate::cmd::serve::cards::CardRoot::open(dir.path(), None)?;
        let db_path = crate::cmd::serve::cards::user_db_path(&root, &dir.path().join("db"))?;
        let db = UserDatabase::open(&db_path)?.collection(id);
        let hash = CardHash::from_hex(&new_hash)?;
        assert!(db.card_exists(hash)?, "the edited card has no row");
        match db.get_card_performance_opt(hash)? {
            Some(Performance::Reviewed(rp)) => assert_eq!(
                rp.review_count, 1,
                "the grade did not land on the edited card"
            ),
            other => {
                return fail(format!(
                    "the edited card was never reviewed: {}",
                    if other.is_some() {
                        "still New"
                    } else {
                        "no row"
                    }
                ));
            }
        }
        Ok(())
    }
}
