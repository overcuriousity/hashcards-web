# MCP Server — Design

An MCP endpoint inside the existing server, so a model can read and write a
user's cards, decks and collections through the same code paths the web UI
uses.

This is Project B of the pair described in
`docs/superpowers/specs/2026-09-08-mcp-server-handoff.md`. Project A — one
review database per user — is done, and this design assumes it: a collection
is a set of rows keyed by `collection_id` in the owning user's single
database, not a file of its own. That is what makes moving a card between
collections an update rather than a cross-database row transfer, and it is
why A came first.

Everything the handoff left open has been decided. The record is in
"Decisions" below; the handoff itself is now history and is not the
authority on anything.

## The shape of the problem

Three things have to be true at once.

**It must run in the same process.** A drill session's card queue lives in
the server's memory. Editing a card changes its hash, and `edit.rs` re-keys
any live session through `migrate_sessions` so the session keeps working. A
separate stdio binary writing the same files would leave a running session
holding dead hashes, with no way to tell it. So the endpoint is a route on
the existing axum server, not a second executable.

**It must not reimplement the domain.** Every write the MCP needs already
exists as a blocking free function that takes the owner and returns
`Fallible<T>`; the axum handler on top of it only turns that into a flash
message and a redirect. The tool layer is an adapter over those functions.
Where a tool duplicated their logic instead, the two would drift, and the
copy that drifted would be the one with no browser exercising it.

**Nothing it deletes may be destroyed.** A write token is a lot of authority
handed to a model, and there is no rate limit. The mitigation is that
deletion is not destruction: everything goes to a trash, and only a human
emptying that trash in the web UI actually erases anything.

## Decisions

Each of these was agreed before the design was written.

1. **A Streamable-HTTP MCP endpoint at `/mcp` inside the existing axum
   process.** Not a separate stdio binary — see above.
2. **The protocol comes from `rmcp`,** the official Rust SDK, version 3.2,
   Apache-2.0. Not hand-rolled. `deny.toml` already allows Apache-2.0. The
   cost is `schemars` and its tree entering the dependency graph; what it
   buys is that protocol revisions and their negotiation are the SDK's
   problem, which removes "a hand-rolled implementation drifts" from the
   risk list entirely.
3. **Auth by a token minted in the web UI** by a logged-in user, not written
   into the config by an administrator. Shown once, stored hashed.
4. **Every token is read-write.** No per-token `write` flag: one kind of
   token, no half-working state to explain or test. The trash carries the
   safety story.
5. **`[mcp] enabled` defaults to on.** A token is required regardless and
   the minting page is behind auth, so "on" is not an open door — while
   "off" means a freshly minted token silently does not work.
6. **Writes cover content and structure; the schedule is read-only.** Cards,
   decks, files, collections, scheduling overrides in `.hashcards.toml` and
   saved decks are writable. No forget, no set-due-date, no suspend: those
   are card states, ROADMAP §2, and doing them here would pre-empt that
   decision.
7. **Tools only, no MCP resources.** `read_deck` and `get_card` already
   return deck and card content; resources would be a second read path to
   keep consistent with the first. Addable later without breaking anything.
8. **~23 explicit tools with good descriptions,** rather than a few
   overloaded ones.
9. **Cards carry their review history when they move between collections.**

## Where the endpoint sits

`rmcp`'s `transport-streamable-http-server` feature provides
`StreamableHttpService`, which implements `tower_service::Service`, so it
nests into the router as it stands:

```rust
let app = app.merge(static_routes);          // server.rs:364, unchanged
let app = app.merge(mcp_routes(&state));     // new, when [mcp] is enabled
```

`mcp_routes` builds `Router::new().nest_service("/mcp", service)` and puts
its own bearer middleware on it.

**It is merged after the `require_auth` layer, exactly where `/auth/*` is
merged** (`server.rs:347`–`362`). The comment there gives the reason for the
auth routes and it applies unchanged here: that layer redirects an
unauthenticated request to `/auth/login`, which is meaningless to an MCP
client. `/mcp` answers `401` with a `WWW-Authenticate: Bearer` header
instead.

Service configuration: `json_response: true` and `legacy_session_mode:
false`, so a plain request-response tool call is answered as
`application/json` with no SSE stream and no session id to carry. The
transport falls back to `text/event-stream` on its own if a handler ever
emits a notification mid-call. Body size is
`StreamableHttpServerConfig::max_request_body_bytes`, not axum's
`DefaultBodyLimit` — the transport enforces its own, defaulting to 4 MiB. It
is set to `MAX_UPLOAD_BYTES` (10 MiB, `upload.rs:18`), the same limit
`/files/media` uses, because `write_deck` sends a whole card file and the
two are the same order of size.

## Identity

### How a token reaches a tool handler

`StreamableHttpService::new` takes a `Fn() -> Result<S, io::Error>` service
factory with no access to the HTTP request, so authentication cannot happen
inside it. It happens in a middleware in front, and travels in request
extensions — which `rmcp` propagates: the transport inserts the
`http::request::Parts` into each request's extensions, and a handler reads
them back through `RequestContext`.

```rust
// middleware, in front of the nested service
let caller = resolve_bearer(&state, headers)?;   // -> McpCaller
req.extensions_mut().insert(caller);

// in a tool handler
let parts = ctx.extensions.get::<http::request::Parts>()...;
let caller = parts.extensions.get::<McpCaller>()...;
```

`McpCaller` is a newtype over `Option<String>` — the owner key, the
lowercased email, `None` when `[oidc]` is absent. That is exactly the
argument every existing domain function already takes, and `None` is already
its "shared `default` tree" case. So the whole of multi-user support in the
MCP is one conversion, and a token minted on an instance with no `[oidc]`
names the `default` tree, the same property that tree already has.

### The token store

A server-level `data_dir/auth.db`, its own SQLite file.

It cannot live in a per-user database: a bearer token has to resolve to a
user *before* the user is known, and searching every user's file for a hash
does not scale and would open every database on every request.

```sql
create table tokens (
    token_hash   text primary key,   -- blake3 of the secret, hex
    owner        text,               -- null when [oidc] is absent
    name         text not null,      -- what the user called it
    created_at   text not null,
    last_used_at text,
    revoked      integer not null default 0
);
```

Only the digest is stored, so a stolen database yields no usable token.
`blake3` is already a direct dependency. `getrandom` — already in the tree
transitively through `openidconnect` — becomes direct, for minting the
secret. Lookup is by primary key, and a hit whose `revoked` is 1 is a miss.
`last_used_at` is written on use.

`AuthDatabase` follows `UserDatabase`: a `parking_lot::Mutex<Connection>`
that is **not reentrant**, so every method takes the lock once at the top
and then calls only free functions taking `&Connection`. This rule is
load-bearing; it is the same one Project A's plan states.

### Minting

A page at `/tokens`, behind `require_auth` like every other UI route: list
your tokens (name, created, last used), mint a new one, revoke one. The
secret is displayed exactly once, on the page that mints it, with the plain
statement that it will not be shown again. Revoking is immediate.

## The adapter seam

This is the part that keeps the MCP from becoming a second implementation of
hashcards. Every mutation the tool surface needs already exists as a
blocking free function of the shape `(&AppState, owner, args) ->
Fallible<T>`:

| Free function | Web handler | Tools built on it |
|---|---|---|
| `create_entry` (`files.rs:397`) | `files_folder_handler`, `files_file_handler` | `create_deck`, `create_collection` |
| `rename_entry` | `files_rename_handler` | `rename_collection`, `move_decks` |
| `delete_entry` (`files.rs:513`) | `files_delete_handler` | `delete_deck`, `delete_collection` |
| `save_file` (`files.rs:806`) | `editor_post_handler` | `write_deck`, `create_card`, `delete_card` |
| `edit_post_inner` (`edit.rs:311`) | `edit_post_handler` | `update_card` |
| `gather_stats` (`stats_page.rs:48`) | `collection_stats_handler` | `get_collection_stats` |
| `persist_custom_decks` (`decks.rs:127`) | `deck_add_handler`, `deck_delete_handler` | `set_saved_deck`, `delete_saved_deck` |
| `existing_collections_for_user` (`files.rs:994`) | `collection_get_handler` | `list_collections`, `get_collection` |

`list_collections` reads through `existing_collections_for_user` rather than
`collections_for_user`, which is the read-path pair the web UI already keeps
apart: the former uses `IdPolicy::ExistingOnly`, so listing collections
cannot write a `.hashcards.toml` into a folder that has none. A tool that is
described as read-only must not create anything.

The work is therefore small and mechanical: raise a few of these to
`pub(crate)`, and replace the `Form`-shaped argument structs of
`create_entry`, `rename_entry`, `delete_entry` and `save_file` with plain
argument structs that both the form extractor and the tool handler can
build. No behaviour moves.

Every tool handler runs its work through `run_blocking` (`cmd/mod.rs:28`),
like every other path that touches SQLite or the tree.

Two consequences fall out for free. Decision 1 is satisfied without any new
code, because `save_file` and `edit_post_inner` already call
`migrate_sessions`, so a card edited over MCP re-keys a running drill
session. And every guard the web UI has — `refuse_if_drilling`,
`check_collection_slug`, the `migration_failures` gate, path validation in
`CardRoot::resolve_entry` — applies to the MCP because it is the same code.

## Deletion, and the trash

### One semantic, not two

Deleting a collection currently erases its review rows:
`delete_entry` calls `remove_collection_rows` (`files.rs:560`), added in
Project A's Task 7 for a good reason — a folder recreated under the same
name must not silently inherit a stale history.

The trash needs the opposite: rows left behind, because card hashes are
content addresses, so a restored folder finds its own rows again and gets
its whole review history back with no dump to write or replay. Every read
path already ignores rows with no card (`stats_page.rs:49` says why, for the
forecast).

**Both routes go through the trash.** Not the MCP alone: one deletion
semantic for the product, or else deleting a collection means different
things depending on which door you came in, which is hard to explain and
harder to test. So:

- `delete_entry` moves the target into the trash instead of unlinking it,
  and no longer erases rows.
- Emptying the trash in the web UI is what erases: it deletes the bytes and
  calls `remove_collection_rows` for a trashed collection. Task 7's concern
  is satisfied there — a collection recreated after a purge starts fresh.
- `delete_entry`'s refusal of a non-empty folder is lifted. It exists
  because "deleting a whole collection on a misclick would take its review
  history with it", and with a trash in the way a misclick no longer does.
  The web UI keeps a confirmation prompt; the refusal itself goes.

### Layout

```
{data_dir}/trash/{tree-name}/{timestamp}-{slug}/
    manifest.toml     kind, collection_id, original relative path, deleted_at
    <the removed bytes, at their original relative shape>
```

Per tree, so one user's trash is not another's, keyed the same way
`{data_dir}/cards/{tree}` and `{data_dir}/db/{tree}.db` are.

Restoring reads `manifest.toml`, refuses if something now occupies the
original path, and moves the bytes back. For a collection this also restores
its `.hashcards.toml` and therefore its `collection_id`, which is what makes
the orphaned rows address themselves again.

### The MCP gets no purge

`list_trash` and `restore_from_trash` only. Emptying the trash is a human
action in the web UI. This is what makes "the model cannot destroy anything
irrecoverably" a property of the design rather than a hope.

## Tool surface

Twenty-three tools. Explicit rather than overloaded: a model picks a
well-named tool more reliably than it fills in a mode parameter, and each
description is a place to teach it the domain.

**Read (7)** — `list_collections`, `get_collection`, `read_deck`,
`list_cards`, `get_card`, `get_collection_stats`, `get_user_stats`.

`list_cards` takes filters — deck, due-only, has-history, a text `query` —
and paginates with a cursor. `search_cards` is folded into it as that
`query` parameter rather than being its own tool. `get_card` returns the
source block, plain-text front and back, stats and review history.
`get_collection_stats` is `gather_stats` as it stands; `get_user_stats` is
cheap now that one database holds every collection.

**Cards (3)** — `create_card`, `update_card`, `delete_card`.

**Decks and files (4)** — `create_deck`, `write_deck` (whole-file replace,
through `save_file`), `move_decks`, `delete_deck`.

**Collections (4)** — `create_collection`, `rename_collection`,
`delete_collection`, `set_collection_scheduling` (the overrides in
`.hashcards.toml`).

**Saved decks (3)** — `list_saved_decks`, `set_saved_deck`,
`delete_saved_deck`, rewriting `hashcards.toml` through
`persist_custom_decks`.

**Trash (2)** — `list_trash`, `restore_from_trash`.

Deliberately absent: `split_collection` and `merge_collections`. Splitting
is `create_collection` plus `move_decks`; merging is `move_decks` plus
`delete_collection`. Two conveniences that could only add ways to be wrong.

## Teaching the model the domain

The `initialize` result's `instructions` field carries what a model cannot
infer from tool names:

- The taxonomy: user → collection → deck → card. A collection is a top-level
  folder in the user's tree; a deck is a markdown file in it; a card is a
  block in that file.
- A card hash is a **content address**. Editing a card changes its hash.
  This is why `update_card` takes the old hash and why a stale hash fails.
- The card syntax: `Q:` / `A:` for a basic card, `C:` with `[cloze]`
  deletions, `---` between cards, TOML frontmatter whose `name` overrides
  the deck name.

Tool descriptions repeat the syntax wherever a tool takes card text, because
a model reading one tool's schema may never have read the instructions.

## Concurrency

Content addressing does most of the work. `update_card` takes a hash; if the
hash no longer resolves, the card has been changed or moved, and the call
fails saying so. No token has to be handed to the model.

The remaining window is between reading a file and writing it. The handler
reads the mtime itself and passes it into `splice_card_block`, whose
re-check just before the rename closes it — the same mechanism
`edit_post_inner` already uses, not a new one.

## Configuration

```toml
[mcp]
enabled = true   # the default
```

`McpSection` parses alongside `OidcSection` in `config.rs` and resolves onto
`ResolvedServeConfig`. Absent section means enabled.

## Changes to existing code

- `config.rs` — `McpSection`, resolved onto `ResolvedServeConfig`.
- `server.rs` — merge `mcp_routes` after the `require_auth` layer; open
  `auth.db` at startup, alongside the existing `ensure_dir` calls.
- `files.rs` — `create_entry`, `rename_entry`, `delete_entry`, `save_file`
  take plain argument structs and become `pub(crate)`; `delete_entry`
  trashes instead of unlinking, stops calling `remove_collection_rows`, and
  loses the non-empty refusal.
- `edit.rs` — `edit_post_inner` becomes `pub(crate)`.
- `stats_page.rs`, `decks.rs`, `cards.rs` — visibility only.
- New: `src/cmd/serve/mcp/` (routing, bearer middleware, tools),
  `src/cmd/serve/tokens.rs` (the minting UI), `src/auth_db.rs`,
  `src/cmd/serve/trash.rs`.
- `Cargo.toml` — `rmcp` 3.2 with its default features (`server`, `macros`,
  `base64`) plus `transport-streamable-http-server`; `getrandom` direct.
- `CHANGELOG.xml` — entries under `<unreleased>`.
- `CLAUDE.md` — the MCP endpoint, the trash, and the stale mention of "git,
  HedgeDoc" in `src/cmd/serve/`, which the remove-remote-sources work left
  behind and `grep` no longer finds.

## Tests

- **`auth_db.rs`** — mint, look up, revoke, `last_used_at`, a revoked token
  is a miss, an unknown digest is a miss, the lock is taken once per method.
- **Bearer middleware** — no header, malformed header, unknown token,
  revoked token each give `401` with `WWW-Authenticate`; a good token
  resolves to the right owner; a token on a no-`[oidc]` instance resolves to
  `None`.
- **Routing** — `/mcp` is reachable without a session cookie and is not
  redirected to `/auth/login`; it is absent when `[mcp] enabled = false`.
- **Protocol** — `initialize`, `tools/list`, `tools/call` over the real
  service, driven by `rmcp`'s own client, so the test exercises negotiation
  rather than a hand-written frame.
- **Each tool** — one test per tool against a temporary tree: it does the
  thing, and it refuses the obvious wrong thing (another user's collection,
  a stale hash, a path with `..`).
- **Isolation** — a token for user A cannot read or write user B's tree.
  This gets its own test rather than riding along on the per-tool ones.
- **Trash** — delete puts bytes in the trash and leaves rows; restore brings
  the collection back *and* its review history, which is the whole point of
  the orphan-row trick and the one test that proves it; purge erases both;
  restore refuses when the path is occupied.
- **Regression, web UI** — the existing file-manager delete tests are
  rewritten for trashing rather than unlinking. This is the one place the
  suite count moves for a reason other than addition, and the plan will
  account for it exactly.
- **Session re-keying** — a card edited through `update_card` re-keys a
  running drill session, mirroring the existing `edit.rs` test.

## Risks

**Card text is untrusted input read by a model.** A collection can hold
anything, including text shaped like instructions, and `get_card` hands it
straight to a model that is also holding a write token. Nothing in this
design prevents that — sanitising prose would break the product, since the
prose *is* the product. It is stated here so it is a known property rather
than a discovery. The trash is what bounds the damage.

**A write token can restructure a lot very quickly.** No rate limit. The
trash and the absence of a purge tool are the whole mitigation, which is why
the purge stays a human action.

**A token is a bearer credential over HTTP.** It is only as safe as the
transport, and the instance is expected to be behind TLS. Worth saying on
the minting page.

**`schemars` enters the dependency graph.** The price of not hand-rolling
the protocol. `deny.toml` gates the licences; the tree is checked when the
dependency lands, not assumed.

## Out of scope

- **MCP resources.** Decision 7. Addable later.
- **Writing the schedule** — forget, set due date, suspend. ROADMAP §2.
- **A purge tool.** By design, permanently.
- **`split_collection`, `merge_collections`, `search_cards`.** Compositions
  of tools that exist, or a parameter on one.
- **Rate limiting.**
- **`SessionDbs` routing cards by hash** (`drill/state.rs:61`): a saved deck
  spanning two collections that hold the same card hash routes arbitrarily.
  Real, pre-existing, noted in Project A's spec, and neither caused nor
  worsened here.
