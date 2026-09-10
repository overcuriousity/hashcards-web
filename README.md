# hashcards-web

[![Test](https://github.com/overcuriousity/hashcards-web/actions/workflows/test.yaml/badge.svg)](https://github.com/overcuriousity/hashcards-web/actions/workflows/test.yaml)
[![Release](https://github.com/overcuriousity/hashcards-web/actions/workflows/release.yaml/badge.svg?branch=master)](https://github.com/overcuriousity/hashcards-web/actions/workflows/release.yaml)
[![dependency status](https://deps.rs/repo/github/overcuriousity/hashcards-web/status.svg)](https://deps.rs/repo/github/overcuriousity/hashcards-web)

![Screenshot of the app, showing a front/back flashcard.](screenshot.webp)

A multi-user web server for plain text spaced repetition. Point it at a
directory of Markdown files and it serves them as flashcard collections,
scheduling reviews with [FSRS] and keeping every user's history in SQLite.

- **Plain text, in your own files.** Cards are Markdown you write in your own
  editor, or in the browser — the server owns the bytes either way, so a card
  can be edited wherever you happen to be looking at it.
- **Content addressed.** A card is identified by the hash of its text, so
  editing a card is a deliberate act with visible consequences for its
  schedule — nothing is silently rewritten behind your back.
- **Multi-user.** With an `[oidc]` section, every route is gated behind
  login and each collection belongs to exactly one owner.
- **Nothing to install for readers.** Reviewing happens in any browser.
  There is no client, no sync protocol, and no account to create.

This is a fork of [hashcards] by [Fernando Borretti][fb], which is a local
command-line tool. The card format, the parser, the FSRS implementation and
the review schema are all his work; see [Credits](#credits). This fork removed
the command-line interface and grew the server: per-user card trees, an
in-browser editor, cross-collection decks and OIDC login.

## Quick start

```bash
$ curl -fsSL https://raw.githubusercontent.com/overcuriousity/hashcards-web/master/install.sh | sh
$ cp hashcards.example.toml hashcards.toml   # then edit it
$ hashcards-web --config hashcards.toml
```

The server reads everything from the configuration file — the bind address,
the data directory, the scheduling defaults, the login settings. There are no
other command-line options:

```
hashcards-web [--config <path>]
```

With no `--config`, `hashcards.toml` in the current directory is used. If
there is no configuration file, the server refuses to start rather than
guessing.

## Installation

### From a release

```bash
$ curl -fsSL https://raw.githubusercontent.com/overcuriousity/hashcards-web/master/install.sh | sh
```

Installs the latest release binary to `~/.local/bin` (override with
`HASHCARDS_INSTALL_DIR`). Linux amd64 and macOS arm64 install this way;
Windows amd64 is published as a `.zip` asset to download from the
[releases page][releases].

### From source

Requires a [Rust toolchain][rustup] and `make` (which downloads and trims the
vendored KaTeX distribution):

```bash
$ make
$ sudo make install          # installs to /usr/local/bin
```

`make example` serves the bundled example collection at
<http://127.0.0.1:8000> so you can see the thing running before writing any
configuration.

## Configuration

`hashcards.example.toml` is the annotated reference; this section explains
what each part is for. `[server].data_dir` is the only required setting —
everything else has a working default, and collections are not configured here
at all: a collection is a folder you create in My Cards.

### `[server]`

```toml
[server]
host = "127.0.0.1"                  # default; see the warning below
port = 8000
data_dir = "/var/lib/hashcards"     # required
session_timeout_minutes = 1440      # 0 disables eviction
```

`data_dir` is where the server keeps the card trees (`{data_dir}/cards/{user}`)
and the review databases (`{data_dir}/db`). A collection is a top-level folder
in one of those trees — discovered by reading the directory, not declared in
this file — and its rows in its owner's review database are scoped by the
stable id in the folder's `.hashcards.toml`, so renaming a folder keeps its
history.

Ownership is structural: with `[oidc]` configured a user's collections are the
folders in `{data_dir}/cards/{their-email-slug}-{hash}/`, and without it they
are the folders in `{data_dir}/cards/default/`. The hash is eight characters
of the email's own digest, because slugifying alone is not injective and the
tree a folder sits in is who owns it. There is no `owner` to declare and no
way to name a collection nobody can reach.

The server creates all of these at startup, and refuses to start with a message
naming the directory if it cannot. **It must be writable by the user the server
runs as.** `/var/lib/hashcards` is the conventional choice for a system
service, but nothing creates it for you unless the systemd unit says so:

```ini
[Service]
User=hashcards
StateDirectory=hashcards          # creates /var/lib/hashcards owned by User=
WorkingDirectory=/var/lib/hashcards
ExecStart=/usr/local/bin/hashcards-web --config /etc/hashcards/hashcards.toml
```

Without `StateDirectory=` (or an equivalent `mkdir` + `chown`), `/var/lib` is
root-owned and the server cannot write there. Running as your own user? Point
`data_dir` somewhere you already own, such as `~/.local/share/hashcards`.

`session_timeout_minutes` evicts drill sessions left idle that long and closes
their database session row. Nothing is lost: every grade is written the moment
it happens, so an evicted session keeps all its progress.

**On binding to the network.** The default `127.0.0.1` is reachable only from
the machine itself. Setting `host = "0.0.0.0"` exposes the server, and without
an `[oidc]` section there is no authentication whatsoever — anyone who can
reach the port can read your cards and edit the underlying files. Expose it
only behind an authenticating reverse proxy, or configure OIDC.

### `[defaults]`

```toml
[defaults]
answer_controls = "full"            # "full" or "binary"
bury_siblings = true
jitter = 0.05
desired_retention = 0.9
max_interval_days = 256
```

Every key has the default shown, so the whole section may be omitted.

- `answer_controls`: `"full"` shows four grading buttons (Forgot / Hard /
  Good / Easy); `"binary"` shows only Forgot and Good.
- `bury_siblings`: show at most one card per cloze group per session, so one
  deletion's text does not spoil its sibling's answer.
- `jitter`: random ±fraction applied to review intervals to spread review
  peaks. `0.0` to `0.5`.
- `desired_retention`: the chance FSRS aims for that a card is still
  remembered when it comes back. `0.7` to `0.99`. Raising it shortens every
  interval — more reviews, more of them remembered; lowering it lengthens
  them. This is the one FSRS number worth having an opinion about.
- `max_interval_days`: the furthest ahead a review may be scheduled. `1` to
  `36500`. The default of `256` was a hardcoded constant before it was
  configurable; a collection you intend to keep for years wants more.

`desired_retention` and `max_interval_days` may be overridden per collection
in that collection's `.hashcards.toml`:

```toml
id = "a1b2c3d4"          # written by the server; leave it alone
desired_retention = 0.95
max_interval_days = 90
```

Either key may be omitted, in which case the value from `[defaults]` applies.
A value out of range, or one written as something other than a number, is
ignored with a warning rather than hiding the collection; so is the rest of
the file if TOML cannot parse it, as long as the `id` line is still legible.
The `id` is the one thing that cannot be recovered by hand — it names the
collection's review database — so a file with no readable `id` at all is left
alone and the collection is skipped with a warning rather than being given a
fresh id on top of your edits.

`jitter`, `answer_controls` and `bury_siblings` are not overridable per
collection. Jitter spreads one person's review peaks across everything they
study, so one collection deciding it alone would be deciding nothing. A card
is always scheduled by the collection that holds it, including when it is
drilled inside a deck spanning several.

### `[oidc]`

```toml
[oidc]
issuer_url = "https://cloud.example.com"
client_id = "..."
client_secret = "..."
external_url = "https://hashcards.example.com"
session_secret = "..."              # at least 32 bytes
# scopes = ["openid", "email", "profile"]
```

Adding this section turns on login for every route except `/auth/*`, and
requires every `[[deck]]` entry to declare
an `owner` — an email, matched case-insensitively against the OIDC `email`
claim. Config load fails if any entry is missing one, and equally if an `owner`
appears *without* an `[oidc]` section, since nobody would ever be logged in to
match it.

- `issuer_url` is the issuer exactly as the provider's discovery document
  declares it, not the path the document happens to be served from. For
  Nextcloud that is the bare base URL. Check with
  `curl {issuer_url}/.well-known/openid-configuration` and use the `issuer`
  field you get back.
- `external_url` is the address a browser actually reaches the server at, even
  behind a reverse proxy. It is independent of `host`/`port`, and the redirect
  URI you register with your provider is `{external_url}/auth/callback`.
- `session_secret` signs the session cookie. It must be at least 32 bytes
  (`openssl rand -hex 32`); config load fails otherwise. Rotating it logs out
  every user.
- The session cookie is `HttpOnly` and `SameSite=Lax`, lasts 30 days (re-issued
  while you keep using it), and is marked `Secure` when `external_url` is
  HTTPS.
- The `email` claim is read from the ID token, or from the UserInfo endpoint
  when the provider only sends it there (Nextcloud does). A provider that
  sends no email at all identifies users by their `sub` claim instead, and an
  `owner` must then be written as that subject.
- Adding a user is a config edit plus a restart. There is no signup flow, no
  admin UI, and no sharing: each collection is visible to exactly one owner. A
  logged-in user who owns nothing sees an empty landing page.
- Log out from the button on the landing page. `/auth/logout` is a POST, so a
  third-party page cannot trigger it.

Without `[oidc]`, the server assumes a single user. Drill sessions are keyed by
collection, so two browsers pointed at the same collection share one session:
both see the same card, and a grade from either advances the shared queue.

### `[[deck]]`

```toml
[[deck]]
name = "Exam revision"
members = ["japanese/Verbs", "medicine-anatomy/Bones"]
# owner = "me@example.com"
```

Two words, kept apart throughout: a **topic** is the cards in one Markdown
file, and a **deck** is a saved selection of topics drawn from any of your
collections and drilled together in one session. Manage decks at `/decks`;
`members` entries are `"{collection-slug}/{topic-name}"` pairs.

A deck owns no cards and no database. Drilling one opens each contributing
collection's own database and routes every review back to the collection the
card came from, so **a card keeps exactly one schedule** however many decks
include it — it never becomes due twice on schedules that drift apart.
Deleting a deck removes only the selection.

## My Cards

**My Cards** (`/files`) is the folder tree hashcards keeps at
`{data_dir}/cards/{user}`, and that only you write to.

Each top-level folder is a collection; each `.md` file inside it is a topic.
Create a folder, add a `.md` file, and write cards in the editor: the buttons
insert Q/A, cloze and term skeletons, and the pane on the right shows the
cards as hashcards parses them. A file that does not parse is never saved —
you get the error and its line number instead.

Renaming a folder is safe. Each one keeps a `.hashcards.toml` holding a stable
id, and review rows are scoped by that id rather than by the folder name, so
your history follows the rename.

A collection folder cannot take the URL slug of a saved deck: both are
addressed through `/collection/{slug}` and routing prefers the collection, so
the deck would become unreachable. Names are rejected when you create or rename
a folder. Two folders whose names produce the same slug are also a collision;
the first by name order wins and the other is left out of the list with a
warning in the log, rather than making the URL mean whichever the filesystem
happened to yield first.

Deleting anything moves it to the **trash** rather than destroying it, so a
folder full of topics can go in one action — you no longer have to empty it
first.

### Trash

`/trash` holds everything you have deleted, from the file manager or through
the MCP endpoint. Restoring puts it back where it was; if something has taken
the same name since, the restore is refused and the copy stays in the trash
rather than overwriting what is there now.

A deleted collection keeps its review history for as long as it is in the
trash. Card hashes are content addresses, so a restored folder finds its own
rows again and its whole schedule comes back — nothing is replayed, because
nothing was thrown away.

**Emptying the trash is the only thing in hashcards that destroys anything.**
It removes the files *and* erases the review history of any collection in
there, which is also what stops a collection recreated under an old name
inheriting a stale schedule. Disk space is not reclaimed until you do it.

### Images

Copy an image and paste it into the editor with Ctrl+V. It is stored under
`{Collection}/media/`, named after a hash of its own bytes — so the same
screenshot pasted twice is stored once, and whatever your screenshot tool
called the file never reaches the disk — and the reference written into the
card is collection-relative (`![](@/media/a1b2c3d4e5f6a7b8.png)`), which
resolves the same from a topic at the top of a collection or one three
folders down. PNG, JPEG, GIF and WebP, up to 10 MB per image; the format is
read from the file's own bytes, not its name. SVG is not accepted: it is
script-bearing markup, and media is served inline from the same origin as the
app.

The `media` folder is hashcards' storage rather than part of your tree, so the
file manager does not list it, and a collection whose topics are all deleted
counts as empty even while their images are still on disk.

## Using it

The landing page lists your collections with the number of cards due. Opening
one shows its topic tree; select topics and start a drill. From there:

| Route | What it does |
|---|---|
| `/` | Collections and saved decks, due counts, one-tap Drill, log out |
| `/files` | My Cards: the folder tree, and the card editor |
| `/collection/{slug}` | Topic tree, duplicate warnings, start a drill |
| `/collection/{slug}/stats` | Due forecast, reviews per day, grades, retention |
| `/collection/{slug}/export` | The whole collection as JSON |
| `/collection/{slug}/bookmarks` | Cards you starred while drilling |
| `/decks` | Create and delete cross-collection decks |

Saved decks appear in the landing list beside collections, tagged `deck`. A
collection with cards due gets a **Drill** button that starts on every topic
at once; the collection name opens the page where the choices live — which
topics, and a cap of 10, 20, 50 or all.

**Keyboard.** During a drill: `space` reveals the answer, `1`–`4` grade it
(Forgot / Hard / Good / Easy), `u` undoes the last grade, and `b` toggles the
bookmark star.

**Editing.** A card can be edited from wherever you are looking at it: a
pencil beside the bookmark star during a drill — shown only once the answer is
revealed, so the editor never puts the answer in front of you while you are
still recalling it — an *Edit* link beside every topic on the collection page,
and the whole-file editor in My Cards. Edits are written straight to the
Markdown file. Because cards are content addressed, editing changes a card's
hash; the server migrates the review history to the new hash where it can and
tells you when it cannot, and a running session follows the card so the grade
after an edit lands on the card in front of you.

Rewording a card keeps its schedule: on save, hashcards matches the new cards
against the old ones by content and carries the review history across.

**Export.** `/collection/{slug}/export` returns every card, its scheduling
state, and the full review history as JSON. Your Markdown is yours on disk,
but the review databases live under `data_dir` — and under OIDC you have no
filesystem access at all — so this is how you get your own history out.

**Duplicates.** Byte-identical cards are deduplicated when a collection loads:
one copy is dropped, and only the other carries review history. The collection
page names any it finds, with both file locations.

## Card format

### Basic cards

```
Q: What are the possible values of electric charge?
A: Any integer multiple of the fundamental charge.
```

Both sides can span multiple lines:

```
Q: List the elements of the Platinum group.
A:

- ruthenium
- rhodium
- palladium
- osmium
- iridium
- platinum
```

### Cloze cards

Cloze cards start with `C:` and use square brackets for deletions:

```
C: The [order] of a group is [the cardinality of its underlying set].
```

They can span multiple lines too:

```
C:
Better is the sight of the eyes than the wandering of the
desire: this is also vanity and vexation of spirit.

— [Ecclesiastes] [6]:[9]
```

Square brackets are reserved for deletions inside `C:` cards. The exact rules:

- `[text]` marks a deletion. It must be non-empty and must close on the same
  line it opens.
- `\[` and `\]` produce literal square brackets.
- Image syntax (`![alt](path)`) is passed through to Markdown untouched.
- Link syntax (`[text](url)`) is passed through untouched: a bracket group
  immediately followed by `(` is a link, not a deletion.
- Nested brackets (`[[a]]`) and deletions left open at the end of a line are
  parse errors.

Each bracketed deletion becomes its own card. A cloze card's hash is derived
from the card's text, the deleted substring, and — when the same substring is
deleted more than once — an occurrence index. It does not depend on byte
offsets or on the machine's CPU architecture, so a database written on one
computer works on any other.

### Term-definition cards

```
T: Monoid
D: A semigroup with an identity element.
```

Shorthand: at parse time this expands into two ordinary cards, one in each
direction.

```
Q: Define: Monoid
A: A semigroup with an identity element.

---

Q: Term for: A semigroup with an identity element.
A: Monoid
```

The generated cards are indistinguishable from hand-written ones — same
content, same hashes — so converting between the shorthand and the explicit
form preserves review history.

A term or a definition may span several lines; a definition runs until the
next `Q:`, `C:`, `T:`, separator or end of file. When one does, its card
puts the prompt on a line of its own so the body stays a block rather than
being folded into the prompt:

```
T: Algorithmus
D:
- präzise, endliche Vorschrift
- endet nach endlich vielen Schritten
```

```
Q: Term for:

- präzise, endliche Vorschrift
- endet nach endlich vielen Schritten
A: Algorithmus
```

Bear in mind that recalling a term from a long, multi-part definition is a
harder exercise than the shorthand's short-definition case, and the prompt
carries the whole definition either way.

Lines starting with `T:` or `D:` are card tags everywhere, exactly like `Q:`
and `A:`. To use such text literally inside a card, don't start a line with it.

### Separators

Cards may optionally be separated by horizontal rules:

```
C: A semigroup with an identity element is called a [monoid].

---

C: A semigroup without associativity is called a [magma].
```

### LaTeX

Math is rendered with KaTeX. Use `$...$` inline and `$$...$$` for display:

```
C: The [amount of substance] of a sample, denoted $n$, is defined as:

$$
n = \frac{N}{N_A}
$$

where $N$ is [the number of elementary entities] and $N_A$ is [Avogadro's constant].
```

Custom macros go in a `macros.tex` file at the collection root, one per line.
Definitions may take arguments (`#1`, `#2`, …):

```
\C \mathbb{C}
\R \mathbb{R}
```

### Images and audio

Ordinary Markdown image syntax works for both:

```
Q: Identify this painting:

![](art/diagram.png)

A: _The Siren_, by John William Waterhouse.
```

```
Q: How do you pronounce "پرنده" in Persian?
A: ![](audio/parande.mp3)
```

Paths resolve relative to the Markdown file containing the card. Prefixing a
path with `@/` resolves it relative to the collection root instead, so the
reference survives the file being moved:

```
cards/
  Art Theory/
    Art.md            # can use Images/Circe.jpg
    Images/
      Circe.jpg       # or @/Art Theory/Images/Circe.jpg from anywhere
```

Media files are validated when a collection loads, and served through
`/collection/{slug}/file/{path}` with path traversal blocked. Audio is
rendered as a player for `.mp3`, `.wav`, `.ogg` and `.m4a`.

### Topic names

A topic is named after its filename: `Medicine.md` is the topic `Medicine`.
Override that with TOML frontmatter:

```
---
name = "Medicine"
---

C: The mitochondria is the [powerhouse] of the cell.
```

This lets many files share one topic name — useful when taking notes from a
book chapter by chapter:

```
Principles of Neural Science/
  Ch1.md
  Ch2.md
```

## MCP

hashcards speaks [MCP](https://modelcontextprotocol.io) at `/mcp`, so an AI
assistant can read and write your cards, decks and collections — writing
cards from your notes, reorganising a collection, or telling you what is
waiting to be reviewed.

Mint a token for yourself at `/tokens`. It is shown once, when it is created,
and only its digest is stored, so nobody can read it back out of the server —
including you. Give one to software you trust, over a connection you trust:
every token can write, and anyone holding one can do anything to your cards
that you can. Revoke it from the same page the moment you no longer want it
to work.

The endpoint is on by default and needs no configuration to reach from the
same machine. An instance served under a real hostname must name it:

```toml
[mcp]
allowed_hosts = ["cards.example.com"]
```

Without that, every MCP request is refused and nothing in the log explains
why. The check is not ours — it is the MCP SDK's protection against a web
page in a browser driving a local MCP server by pointing its own hostname at
`127.0.0.1` — and its default of loopback-only is right for an MCP server
running on a desktop and wrong for one on a server. `enabled = false` turns
the endpoint off entirely.

What an assistant can and cannot do:

- **Cards, decks, collections and saved decks are writable.** So are a
  collection's scheduling settings.
- **The schedule itself is not.** There is no way to make a card be
  forgotten, set its due date, or suspend it. Reading a card's history and
  statistics is fine; rewriting them is not on offer.
- **Nothing it deletes is destroyed.** Everything goes to your trash, and
  there is no tool that empties it — that is a human action, in the web
  interface. This is the reason a write token is a reasonable thing to hand
  out at all.

One thing worth knowing: your cards are text, and an assistant reading them
is reading text you may not have written. A collection imported from
somewhere else could contain something shaped like an instruction. That is
worth a thought before pointing an assistant with a write token at a
collection you did not write yourself — the trash is what bounds the damage,
not prevention.

## Database

Each **user** has one SQLite database at `{data_dir}/db/{tree}.db`, where
`tree` is the name of their card tree under `{data_dir}/cards/` — `default`
without `[oidc]`, and `{email-slug}-{hash}` with it. Every row carries the
`collection_id` of the collection it belongs to: the stable id in that
collection folder's `.hashcards.toml`, not the folder name, so renaming a
folder keeps its history.

Identical cards in two collections keep two schedules. That is what two
files necessarily meant, and it is now stated as the primary key
`(collection_id, card_hash)`.

Reviews are written as they happen and in the same transaction as the card's
performance, so an interrupted session keeps its progress. Undo marks a
review `voided` rather than deleting it, and read paths filter on
`voided = 0`.

Databases are opened with write-ahead logging where the filesystem supports
it. Where it does not — some NFS and SMB mounts — hashcards logs a line and
carries on in rollback-journal mode.

### Upgrading from per-collection databases

Before this release, each *collection* had its own database at
`{data_dir}/db/{id}.db`. On the first start after upgrading, those are
merged into one database per user, in a single transaction per user, and the
originals are **moved** — not deleted — into `{data_dir}/db/legacy/`.

A user whose merge fails is refused, loudly: their collections show the
error and point at the startup log, rather than being served from an empty
database. Every other user is served, and the server starts.

**The upgrade is one way.** An older binary will not look at
`db/{tree}.db`; it will look for `db/{id}.db`, find nothing, and quietly
create empty per-collection databases, so every session starts from zero
with no error at all. If you need to go back: stop the server, move the
files in `db/legacy/` back into `db/`, and delete the `db/{tree}.db` files
the newer binary wrote. The originals are kept precisely so that this is
possible — and moved rather than left where they were, because an older
binary writing into a file that has already been merged would have those
reviews skipped by the next upgrade and lost without a word.

The `cards` table:

| Column             | Type               | Description                                                                        |
|--------------------|--------------------|------------------------------------------------------------------------------------|
| `collection_id`    | `text not null`    | The collection this row belongs to: the stable id in its folder's `.hashcards.toml`. |
| `card_hash`        | `text not null`    | The hash of the card.                                                              |
| `added_at`         | `text not null`    | When the card was first added to the database.                                     |
| `last_reviewed_at` | `text`             | When the card was most recently reviewed. `null` if the card is new.               |
| `stability`        | `real`             | The card's stability. `null` if the card is new.                                   |
| `difficulty`       | `real`             | The card's difficulty. `null` if the card is new.                                  |
| `interval_raw`     | `real`             | The FSRS-calculated interval, before rounding and clamping, in days.               |
| `interval_days`    | `integer`          | The interval in whole days, after rounding and clamping.                           |
| `due_date`         | `text`             | When the card is next due, `YYYY-MM-DD`. `null` if the card is new.                |
| `review_count`     | `integer not null` | How many times the card has been reviewed.                                         |

The `sessions` table:

| Column         | Type                  | Description                                                                     |
|----------------|-----------------------|---------------------------------------------------------------------------------|
| `session_id`   | `integer primary key` | The ID of the session.                                                          |
| `collection_id` | `text not null`      | The collection this row belongs to: the stable id in its folder's `.hashcards.toml`. |
| `started_at`   | `text not null`       | When the session started.                                                       |
| `ended_at`     | `text not null`       | When the session ended.                                                         |
| `last_seen_at` | `text`                | Stamped as the owning process serves the session, so the startup sweep can tell a session abandoned by a crash from one still live elsewhere. |
| `closed`       | `integer not null`    | Whether the row has been closed. A session whose reviews were all undone is rewritten back to `ended_at = started_at`, so that cannot serve as the marker. |

The `reviews` table:

| Column          | Type                  | Description                                                                     |
|-----------------|-----------------------|---------------------------------------------------------------------------------|
| `review_id`     | `integer primary key` | The review ID.                                                                  |
| `session_id`    | `integer not null`    | The session this review was performed in, a foreign key.                        |
| `collection_id` | `text not null`       | The collection this row belongs to: the stable id in its folder's `.hashcards.toml`. |
| `card_hash`     | `text not null`       | The card that was reviewed, a foreign key.                                      |
| `reviewed_at`   | `text not null`       | When the grade was submitted.                                                   |
| `grade`         | `text not null`       | One of `forgot`, `hard`, `good`, or `easy`.                                     |
| `stability`     | `real not null`       | The card's stability after this review.                                         |
| `difficulty`    | `real not null`       | The card's difficulty after this review.                                        |
| `interval_raw`  | `real not null`       | The FSRS-calculated interval, before rounding and clamping, in days.            |
| `interval_days` | `integer not null`    | The interval in whole days, after rounding and clamping.                        |
| `due_date`      | `text not null`       | When the card is next due, `YYYY-MM-DD`.                                        |
| `duration_ms`   | `integer`             | How long the card was on screen, when that is known.                            |
| `voided`        | `integer not null`    | Set by undo. Read paths filter on `voided = 0`; the row itself is never deleted. |
| `reviewed_date` | `text` (generated)    | The date half of `reviewed_at`, indexed so the stats page can group by day.     |

The `bookmarks` table:

| Column       | Type               | Description                                                    |
|--------------|--------------------|----------------------------------------------------------------|
| `collection_id` | `text not null` | The collection this row belongs to: the stable id in its folder's `.hashcards.toml`. |
| `card_hash`  | `text not null`    | The bookmarked card, a foreign key that cascades on rename.    |
| `note`       | `text`             | The note attached to the bookmark, if any.                     |
| `created_at` | `text not null`    | When the bookmark was made.                                    |

Two more tables are hashcards' own bookkeeping and hold no review data:
`schema_version`, which drives the migrations run at open, and `meta`.

Timestamps are `YYYY-MM-DDTHH:MM:SS.MMM`, e.g. `2025-10-04T17:09:51.517`.
Dates are naive by design: a due date is closer to the date on a journal entry
than to a precise point in time, so there are no timezones anywhere.

## Credits

hashcards-web is a fork of [hashcards] by [Fernando Borretti][fb]
([announcement post][blog]), and most of what makes it work is still his: the
card format, the Markdown parser, the FSRS implementation, the review schema
and the database design. His [essay on effective spaced repetition][esr]
explains the reasoning behind the whole thing, and is worth reading before you
write your first card.

The fork is maintained by [overcuriousity], with [mstoeck3] — it removed the
command-line interface and grew the server around what was left: My Cards and
the in-browser editor, cross-collection decks, OIDC login and per-user card
trees, bookmarks, the statistics page, JSON export, and configurable
scheduling. The upstream project also has its
[own contributors](https://github.com/eudoxia0/hashcards/graphs/contributors),
whose work came across with the fork; the fork's are listed
[here](https://github.com/overcuriousity/hashcards-web/graphs/contributors).

hashcards-web vendors [KaTeX] for maths, [highlight.js] for code blocks, and
the [Inter] and [JetBrains Mono] typefaces. Scheduling is [FSRS].

## Prior art

Other plain-text and file-backed spaced repetition systems, for comparison:

- [org-fc](https://github.com/l3kn/org-fc)
- [org-drill](https://orgmode.org/worg/org-contrib/org-drill.html)
- [hascard](https://hackage.haskell.org/package/hascard)
- [carddown](https://github.com/martintrojer/carddown)
- [My implementation of a personal mnemonic medium](https://notes.andymatuschak.org/My_implementation_of_a_personal_mnemonic_medium)

[FSRS]: https://github.com/open-spaced-repetition/fsrs4anki
[hashcards]: https://github.com/eudoxia0/hashcards
[blog]: https://borretti.me/article/hashcards-plain-text-spaced-repetition
[esr]: https://borretti.me/article/effective-spaced-repetition
[rustup]: https://rustup.rs/
[releases]: https://github.com/overcuriousity/hashcards-web/releases/latest
[overcuriousity]: https://github.com/overcuriousity
[mstoeck3]: https://github.com/mstoeck3
[KaTeX]: https://katex.org/
[highlight.js]: https://highlightjs.org/
[Inter]: https://rsms.me/inter/
[JetBrains Mono]: https://www.jetbrains.com/lp/mono/

## License

© 2025 [Fernando Borretti][fb] and the contributors to hashcards and this
fork. Licensed under the [Apache 2.0][apache2] license.

[fb]: https://borretti.me/
[apache2]: https://www.apache.org/licenses/LICENSE-2.0
