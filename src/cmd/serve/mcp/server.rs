//! The MCP server itself: what it tells a model about hashcards, and how a
//! tool handler learns who is calling.

use rmcp::ErrorData;
use rmcp::RoleServer;
use rmcp::ServerHandler;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::Implementation;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::service::RequestContext;
use rmcp::tool_handler;

use crate::cmd::serve::mcp::auth::McpCaller;
use crate::cmd::serve::state::AppState;

/// What the model is told at `initialize`.
///
/// Everything here is something a model cannot infer from tool names, and
/// getting any of it wrong produces cards that look right and hash
/// differently. Tool descriptions repeat the syntax wherever a tool takes
/// card text, because a model reading one schema may never have read this.
pub const INSTRUCTIONS: &str = "\
hashcards is a spaced-repetition system over plain Markdown files.

The taxonomy, outermost first:

  user -> collection -> deck -> card

A collection is a top-level folder in the user's card tree, addressed by a
slug derived from its folder name. A deck is a Markdown file inside a
collection; the folder structure below the collection is yours to organise.
A card is a block inside a deck file.

A card hash is a CONTENT ADDRESS: it is derived from the card's text, so
editing a card changes its hash. `update_card` takes the hash of the card as
it is now; if that hash no longer resolves, the card has already been changed
or moved, and you should read it again rather than retrying.

Card syntax. Cards in a file are separated by a line containing only `---`.

A basic card is a question and an answer:

    Q: What is the capital of France?
    A: Paris.

Either may run over several lines, in which case the text starts on the line
after the marker.

A cloze card is one text with deletions marked by square brackets. Each
deletion becomes its own card:

    C: The [order] of a group is [the cardinality of its underlying set].

A file may begin with TOML frontmatter between `---` lines. A `name` there
overrides the deck name that would otherwise come from the file name.

Scheduling is read-only. You can read a card's statistics and review history,
but you cannot set a due date, suspend a card, or make it be forgotten.

Nothing you delete is destroyed: it goes to the user's trash, and you can
list and restore from it. Only the user can empty the trash, from the web
interface.";

/// The MCP server: one per connection, cheap to build, holding the same
/// `AppState` every web handler holds.
#[derive(Clone)]
pub struct HashcardsMcp {
    pub state: AppState,
    tool_router: ToolRouter<Self>,
}

impl HashcardsMcp {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            // Groups are added here as later commits land:
            //   Self::read_router() + Self::card_router() + ...
            tool_router: Self::read_router()
                + Self::card_router()
                + Self::deck_router()
                + Self::collection_router()
                + Self::saved_router()
                + Self::trash_router(),
        }
    }

    /// Who is calling, out of the request extensions the bearer middleware
    /// filled in.
    ///
    /// A failure here is a bug rather than a client error -- the middleware
    /// refuses anything it cannot resolve, so reaching a tool without a
    /// caller means the route was mounted without the layer.
    pub fn caller(&self, ctx: &RequestContext<RoleServer>) -> Result<McpCaller, ErrorData> {
        ctx.extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<McpCaller>())
            .cloned()
            .ok_or_else(|| {
                ErrorData::internal_error(
                    "This request arrived without an identity, which should not be possible. \
                     Please report this.",
                    None,
                )
            })
    }

    /// Every tool this server offers, by name. The trash tools' test uses
    /// it to assert that nothing in the whole surface destroys anything.
    #[cfg(test)]
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for HashcardsMcp {
    fn get_info(&self) -> ServerInfo {
        // Built from the default and adjusted rather than written as a
        // literal: `ServerInfo` is `#[non_exhaustive]`, so a field the SDK
        // adds later arrives with its own default instead of breaking the
        // build. The protocol version comes from there too, which is what
        // keeps negotiation the SDK's problem and not ours.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::new("hashcards-web", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(INSTRUCTIONS.to_string());
        info
    }
}
