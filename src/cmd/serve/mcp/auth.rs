//! Who an MCP request is from.
//!
//! `/mcp` sits outside `require_auth` -- that layer redirects to
//! `/auth/login`, which means nothing to an MCP client -- so it does its
//! own bearer check here and answers `401` instead of a redirect.

use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::http::header::WWW_AUTHENTICATE;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;

use crate::auth_db::TokenSecret;
use crate::cmd::run_blocking;
use crate::cmd::serve::auth::CurrentUser;
use crate::cmd::serve::state::AppState;
use crate::types::timestamp::Timestamp;

/// Who an MCP request is from, once its token has been resolved.
///
/// `None` is the shared `default` tree -- an instance with no `[oidc]` --
/// and is a real identity here rather than an absence, exactly as it is
/// everywhere else in the server.
#[derive(Clone)]
pub struct McpCaller {
    // Read by the tool handlers, which land in the next commits.
    #[cfg_attr(not(test), allow(dead_code))]
    owner: Option<String>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl McpCaller {
    /// The same identity in the form every domain function already takes.
    ///
    /// This is the whole of multi-user support in the MCP: past this point
    /// a tool handler calls exactly the function a web handler calls, with
    /// exactly the argument a browser session would have produced.
    pub fn current_user(&self) -> Option<CurrentUser> {
        self.owner.as_deref().map(CurrentUser::new)
    }
}

/// Refuse anything without a live bearer token, and hand the resolved
/// identity to the service behind us in the request's extensions.
pub async fn require_bearer(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(secret) = bearer_token(&headers) else {
        return unauthorized("A bearer token is required. Mint one at /tokens.");
    };
    let Some(auth) = state.auth.clone() else {
        return unauthorized("This server has no token store, so it cannot authenticate you.");
    };
    // SQLite, so not on the async executor.
    let resolved = run_blocking(move || auth.resolve(&secret, Timestamp::now())).await;
    let owner = match resolved {
        Ok(Some(owner)) => owner,
        Ok(None) => return unauthorized("That token is not valid. It may have been revoked."),
        Err(e) => {
            log::error!("Could not check an MCP token: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not check your token.",
            )
                .into_response();
        }
    };
    request.extensions_mut().insert(McpCaller { owner });
    next.run(request).await
}

/// The token out of an `Authorization: Bearer …` header, if it is one and
/// it is shaped like a hashcards token.
fn bearer_token(headers: &HeaderMap) -> Option<TokenSecret> {
    let raw = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let rest = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))?;
    TokenSecret::parse(rest).ok()
}

fn unauthorized(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(WWW_AUTHENTICATE, "Bearer realm=\"hashcards\"")],
        message.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, value.parse().expect("a valid header value"));
        h
    }

    #[test]
    fn a_bearer_token_is_read_out_of_the_header() {
        let secret = TokenSecret::generate().expect("a token");
        let found = bearer_token(&headers(&format!("Bearer {secret}"))).expect("a token");
        assert_eq!(found.digest(), secret.digest());
    }

    /// RFC 9110 says the scheme is case-insensitive, and clients differ.
    #[test]
    fn the_bearer_scheme_is_accepted_in_lower_case() {
        let secret = TokenSecret::generate().expect("a token");
        assert!(bearer_token(&headers(&format!("bearer {secret}"))).is_some());
    }

    #[test]
    fn anything_that_is_not_a_bearer_token_is_ignored() {
        let secret = TokenSecret::generate().expect("a token");
        assert!(bearer_token(&HeaderMap::new()).is_none());
        assert!(bearer_token(&headers(&format!("Basic {secret}"))).is_none());
        assert!(bearer_token(&headers("Bearer not-a-token")).is_none());
        assert!(bearer_token(&headers("Bearer")).is_none());
    }

    /// The conversion that is the whole of multi-user support in the MCP:
    /// past here a tool handler holds exactly what a browser session would
    /// have produced.
    #[test]
    fn a_caller_becomes_the_current_user_the_domain_functions_take() {
        let named = McpCaller {
            owner: Some("me@example.com".to_string()),
        };
        assert_eq!(
            named.current_user().map(|u| u.email),
            Some("me@example.com".to_string())
        );

        // No `[oidc]`: the shared `default` tree, which is an identity
        // rather than an absence.
        let anonymous = McpCaller { owner: None };
        assert!(anonymous.current_user().is_none());
    }
}
