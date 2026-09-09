Dear Claude: this document is to help you in your work.

# Overview

hashcards-web is a multi-user web server for plain-text spaced repetition,
written in Rust. It parses Markdown files containing flashcards, stores
performance data in SQLite, and presents cards through a web interface using
the FSRS algorithm for scheduling. Forked from eudoxia0/hashcards, which was
a local CLI tool; the CLI is gone.

# Design and Internals

- Cards are content addressed.
- One review database per user, at `{data_dir}/db/{tree}.db`. Every row
  carries its `collection_id`. `Database` is a *view* of one collection on a
  `UserDatabase`'s shared connection, and that mutex is not reentrant: a
  method takes the lock once and calls only free functions under it.
- Media files are referenced in markdown using standard image syntax: `![](path/to/file.ext)`. Standard image and AV formats are supported.
- We use `pulldown-cmark` to parse/process/render Markdown.
- In `markdown.rs`: URLs are rewritten to `/file/{url}` endpoints for serving.
- In `media.rs`: Image references are extracted and validated during collection loading.
- Files are served via `/file/*path` endpoint, resolved relative to collection directory.
- Path validation (in `src/media/load.rs`) prevents directory traversal attacks.

# Layout

- One binary, one job: `hashcards-web [--config <path>]`, defaulting to
  `hashcards.toml`. There are no subcommands. A config file is mandatory.
- `src/cmd/serve/` is the server: routing, handlers, auth, config, editing,
  decks, export, and the startup merge of pre-consolidation databases.
- `src/cmd/drill/` is the drill engine the server embeds — rendering
  (`get.rs`), actions (`post.rs`), session state, cache, templates and
  static assets. The directory names predate the fork; there is no drill
  command any more.
- `Collection::new`, `ResolvedServeConfig::from_directories` and
  `wait_for_server` are `#[cfg(test)]`. Do not reach for them in production
  code: the server resolves databases through the config, not by convention.

# Rules

- Use newtypes for domain concepts.
- When fixing bugs, add a failing regression test first.
- No `unwrap()` calls in production code. Tests are ok.
- Use `Fallible` and `?` for error handling.
- Use `fail()` function for creating custom errors.
- All errors are user-facing, so messages should be clear.
- Keep functions small and focused.
- Module files should re-export what's needed, hide implementation details.
- Prefer imports to fully qualified names: e.g. instead of writing `foo::bar()`, add a `use foo::bar;` statement at the top of the module.
- Each grade is written to the database as it happens, so an interrupted session keeps its progress. Undo marks the review `voided` rather than deleting it; read paths filter on `voided = 0`. Write the review and the card's performance in one transaction.
- The cache stays the in-memory working state for the session (rendering, undo). It is no longer the sole writer.
- Don't use timezones: dates are naive for a reason. Due dates etc. are more like the dates in a journal entry than precise points in time.
- When relevant, update `CHANGELOG.xml`.
- When updating this file, be terse.

# Watch Out

- Cloze deletion positions are _byte_ positions, not _character_ positions. Therefore: when working with cloze positions, always use `.bytes()` not `.chars()`.

Thank you. Good luck little buddy.
