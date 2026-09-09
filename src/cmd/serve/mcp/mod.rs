//! The MCP endpoint at `/mcp`.
//!
//! In this process rather than a separate binary, because a drill
//! session's card queue lives in the server's memory: editing a card
//! changes its hash, and only an in-process writer can re-key a running
//! session the way `edit.rs` does.

pub mod auth;
pub mod server;
pub mod tools;

use std::sync::Arc;

use axum::Router;
use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

use crate::cmd::serve::mcp::auth::require_bearer;
use crate::cmd::serve::mcp::server::HashcardsMcp;
use crate::cmd::serve::state::AppState;
use crate::cmd::serve::upload::MAX_UPLOAD_BYTES;

/// `/mcp`, with its own authentication.
///
/// Returned as its own `Router` so `server.rs` can merge it *after* the
/// `require_auth` layer, exactly as the `/auth/*` routes are merged and for
/// the same reason: that layer redirects to `/auth/login`, which means
/// nothing to an MCP client.
pub fn mcp_routes(state: &AppState) -> Router<AppState> {
    let for_service = state.clone();
    let service = StreamableHttpService::new(
        move || Ok(HashcardsMcp::new(for_service.clone())),
        Arc::new(LocalSessionManager::default()),
        mcp_config(state),
    );
    // `route_layer`, not `layer`: `layer` also wraps the router's fallback,
    // so after the merge every unmatched path in the whole application
    // would answer 401 from the bearer check instead of 404.
    Router::new()
        .nest_service("/mcp", service)
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ))
}

/// How the transport is configured.
///
/// Built from the default and adjusted: `StreamableHttpServerConfig` is
/// `#[non_exhaustive]`, so a setting the SDK adds later arrives with its
/// own default rather than breaking the build.
fn mcp_config(state: &AppState) -> StreamableHttpServerConfig {
    let mut config = StreamableHttpServerConfig::default();
    // A tool call is request-response, so it is answered as plain JSON with
    // no stream and no session id to carry. The transport falls back to SSE
    // by itself if a handler ever emits a notification mid-call.
    config.json_response = true;
    config.legacy_session_mode = false;
    // `write_deck` sends a whole card file, which is the same order of size
    // as a pasted image.
    config.max_request_body_bytes = MAX_UPLOAD_BYTES;
    // Loopback plus whatever `[mcp].allowed_hosts` names. Without this an
    // instance on a real hostname refuses every request: the SDK's default
    // is loopback only, as protection against DNS rebinding.
    config.allowed_hosts = state.config.mcp.allowed_hosts.clone();
    config
}
