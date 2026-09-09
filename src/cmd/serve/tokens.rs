//! Minting and revoking MCP tokens.
//!
//! A token is created by the user it belongs to, from a page behind the
//! same login as every other page — not written into the config file by an
//! administrator. The secret is shown once and never stored: the database
//! keeps only its digest.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Form;
use axum::extract::Query;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Html;
use axum::response::Redirect;
use maud::Markup;
use maud::html;
use serde::Deserialize;

use crate::auth_db::AuthDatabase;
use crate::auth_db::TokenHash;
use crate::auth_db::TokenRecord;
use crate::auth_db::TokenSecret;
use crate::cmd::drill::template::page_template;
use crate::cmd::run_blocking;
use crate::cmd::serve::auth::CurrentUser;
use crate::cmd::serve::state::AppState;
use crate::error::Fallible;
use crate::error::fail;
use crate::flash::Flash;
use crate::types::timestamp::Timestamp;

/// The caller's owner key, lowercased as it is stored.
fn owner_key(user: Option<&CurrentUser>) -> Option<String> {
    user.map(|u| u.email.to_lowercase())
}

fn auth_db(state: &AppState) -> Fallible<Arc<AuthDatabase>> {
    match &state.auth {
        Some(a) => Ok(Arc::clone(a)),
        None => fail("No data directory is configured, so tokens cannot be stored."),
    }
}

fn list_tokens(state: &AppState, user: Option<&CurrentUser>) -> Fallible<Vec<TokenRecord>> {
    auth_db(state)?.list(owner_key(user).as_deref())
}

fn mint_token(state: &AppState, user: Option<&CurrentUser>, name: &str) -> Fallible<TokenSecret> {
    auth_db(state)?.mint(owner_key(user).as_deref(), name, Timestamp::now())
}

fn revoke_token(state: &AppState, user: Option<&CurrentUser>, raw: &str) -> Fallible<String> {
    let hash = TokenHash::parse(raw)?;
    if auth_db(state)?.revoke(owner_key(user).as_deref(), &hash)? {
        Ok("Token revoked.".to_string())
    } else {
        fail("That token is not one of yours, or it was already revoked.")
    }
}

/// `secret` is `Some` only on the response to the request that minted it.
fn render_tokens(
    tokens: &[TokenRecord],
    secret: Option<&TokenSecret>,
    flash: Option<Flash>,
) -> Markup {
    page_template(html! {
        div.landing {
            @if let Some(f) = &flash { (f.render()) }
            div.browse-header {
                a.back-link href="/" { "← Collections" }
                h1 { "MCP tokens" }
            }

            p.hint {
                "A token lets an MCP client read and write your cards, decks and collections. \
                 Give one to software you trust, over a connection you trust: anyone holding \
                 it can do anything to your cards that you can."
            }

            @if let Some(secret) = secret {
                div.notice {
                    p { strong { "Copy this now — it will not be shown again." } }
                    pre { code { (secret) } }
                }
            }

            form.add-source-form action="/tokens/new" method="post" {
                div.add-source-row {
                    input.input.add-source-url type="text" name="name"
                        placeholder="What is this token for? e.g. laptop" required;
                    input.btn.btn-primary type="submit" value="Mint a token";
                }
            }

            @if tokens.is_empty() {
                p.notice { "You have no tokens." }
            } @else {
                ul.file-tree {
                    @for token in tokens {
                        li.file-row {
                            span.file-name { (token.name) }
                            span.hint {
                                "created " (token.created_at) ", "
                                @match &token.last_used_at {
                                    Some(t) => { "last used " (t) }
                                    None => { "never used" }
                                }
                            }
                            div.file-actions {
                                form.file-form action="/tokens/revoke" method="post" {
                                    input type="hidden" name="id" value=(token.hash);
                                    input.btn.btn-sm.btn-danger type="submit" value="Revoke";
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

pub async fn tokens_get_handler(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    current_user: Option<CurrentUser>,
) -> (StatusCode, Html<String>) {
    let flash = Flash::from_query(&query);
    // SQLite, so not on the async executor (BUG-44).
    let tokens = run_blocking(move || list_tokens(&state, current_user.as_ref())).await;
    let markup = match tokens {
        Ok(tokens) => render_tokens(&tokens, None, flash),
        Err(e) => render_tokens(&[], None, Some(Flash::error(e.to_string()))),
    };
    (StatusCode::OK, Html(markup.into_string()))
}

#[derive(Deserialize)]
pub struct MintForm {
    pub name: String,
}

/// Answers with the page rather than redirecting: a redirect would have to
/// carry the secret in a URL, where it would land in the browser's history
/// and in every access log between here and there.
pub async fn tokens_mint_handler(
    State(state): State<AppState>,
    current_user: Option<CurrentUser>,
    Form(form): Form<MintForm>,
) -> (StatusCode, Html<String>) {
    let outcome = run_blocking(move || {
        let secret = mint_token(&state, current_user.as_ref(), &form.name)?;
        let tokens = list_tokens(&state, current_user.as_ref())?;
        Ok((secret, tokens))
    })
    .await;
    match outcome {
        Ok((secret, tokens)) => (
            StatusCode::OK,
            Html(render_tokens(&tokens, Some(&secret), None).into_string()),
        ),
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Html(render_tokens(&[], None, Some(Flash::error(e.to_string()))).into_string()),
        ),
    }
}

#[derive(Deserialize)]
pub struct RevokeForm {
    pub id: String,
}

pub async fn tokens_revoke_handler(
    State(state): State<AppState>,
    current_user: Option<CurrentUser>,
    Form(form): Form<RevokeForm>,
) -> Redirect {
    match run_blocking(move || revoke_token(&state, current_user.as_ref(), &form.id)).await {
        Ok(msg) => Flash::success(msg).redirect("/tokens"),
        Err(e) => Flash::error(e.to_string()).redirect("/tokens"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::serve::state::test_support::state_with_data_dir;
    use tempfile::TempDir;

    fn fixture() -> Fallible<(TempDir, AppState)> {
        let dir = TempDir::new()?;
        let mut state = state_with_data_dir(dir.path().to_path_buf());
        state.auth = Some(Arc::new(AuthDatabase::open(&dir.path().join("auth.db"))?));
        Ok((dir, state))
    }

    #[test]
    fn minting_returns_a_usable_secret() -> Fallible<()> {
        let (_dir, state) = fixture()?;
        let secret = mint_token(&state, None, "laptop")?;
        let auth = auth_db(&state)?;
        assert_eq!(
            auth.resolve(&TokenSecret::parse(&secret.to_string())?, Timestamp::now())?,
            Some(None)
        );
        Ok(())
    }

    #[test]
    fn a_token_needs_a_name() -> Fallible<()> {
        let (_dir, state) = fixture()?;
        assert!(mint_token(&state, None, "   ").is_err());
        Ok(())
    }

    /// The secret appears on the page that mints it, and that page says it
    /// will not appear again.
    #[test]
    fn the_secret_is_shown_once_with_a_warning() -> Fallible<()> {
        let (_dir, state) = fixture()?;
        let secret = mint_token(&state, None, "laptop")?;
        let html = render_tokens(&list_tokens(&state, None)?, Some(&secret), None).into_string();
        assert!(
            html.contains(&secret.to_string()),
            "the secret is not on the page"
        );
        assert!(html.contains("will not be shown again"), "{html}");
        Ok(())
    }

    /// Every later render of the page must not carry it.
    #[test]
    fn the_secret_is_not_on_the_listing_page() -> Fallible<()> {
        let (_dir, state) = fixture()?;
        let secret = mint_token(&state, None, "laptop")?;
        let html = render_tokens(&list_tokens(&state, None)?, None, None).into_string();
        assert!(
            !html.contains(&secret.to_string()),
            "the secret is still on the page"
        );
        assert!(html.contains("laptop"), "{html}");
        Ok(())
    }

    #[test]
    fn revoking_makes_the_token_stop_working() -> Fallible<()> {
        let (_dir, state) = fixture()?;
        let secret = mint_token(&state, None, "laptop")?;
        revoke_token(&state, None, &secret.digest().to_string())?;
        let auth = auth_db(&state)?;
        assert_eq!(auth.resolve(&secret, Timestamp::now())?, None);
        assert!(list_tokens(&state, None)?.is_empty());
        Ok(())
    }

    #[test]
    fn a_malformed_token_id_is_refused() -> Fallible<()> {
        let (_dir, state) = fixture()?;
        assert!(revoke_token(&state, None, "not-a-digest").is_err());
        Ok(())
    }

    /// Without a data directory there is nowhere to keep tokens, and the
    /// page has to say so rather than panicking on an absent store.
    #[test]
    fn a_server_with_no_token_store_says_so() -> Fallible<()> {
        let (_dir, mut state) = fixture()?;
        state.auth = None;
        let err = mint_token(&state, None, "laptop").unwrap_err();
        assert!(
            err.message().contains("data directory"),
            "{}",
            err.message()
        );
        Ok(())
    }
}
