pragma foreign_keys = on;

-- One user's review database. Every table is scoped by `collection_id`, the
-- stable id in a collection folder's `.hashcards.toml`. The grain used to be
-- one file per collection, which made moving a card between collections a
-- cross-database row transfer and left a database nothing could attribute
-- behind whenever a folder was deleted from outside the application.

create table cards (
    collection_id text not null,
    card_hash     text not null,
    added_at text not null,
    last_reviewed_at text,
    stability real,
    difficulty real,
    interval_raw real,
    interval_days integer,
    due_date text,
    review_count integer not null,
    -- Identical cards in two collections keep two schedules, which is
    -- exactly what two files meant. This is also the key the reviews and
    -- bookmarks foreign keys cascade through.
    primary key (collection_id, card_hash)
) strict;

create table sessions (
    session_id integer primary key,
    -- New information: a session used to be identified by which file it was
    -- written in.
    collection_id text not null,
    started_at text not null,
    ended_at text not null,
    -- Heartbeat: stamped whenever the owning process serves a page or
    -- handles an action. Lets the startup sweep tell a session abandoned by
    -- a crash from one still live in another process.
    last_seen_at text,
    -- Explicit "this row has been closed" marker. `ended_at = started_at`
    -- cannot serve as one: a session whose reviews were all undone is
    -- rewritten back to that value and would be re-detected forever.
    closed integer not null default 0
) strict;

create table reviews (
    review_id integer primary key,
    session_id integer not null
        references sessions (session_id)
        on update cascade
        on delete cascade,
    collection_id text not null,
    card_hash text not null,
    reviewed_at text not null,
    grade text not null,
    stability real not null,
    difficulty real not null,
    interval_raw real not null,
    interval_days integer not null,
    due_date text not null,
    duration_ms integer,
    voided integer not null default 0,
    reviewed_date text generated always as (substr(reviewed_at, 1, 10)) virtual,
    -- ON UPDATE CASCADE is load-bearing twice over: an edit renames a card's
    -- hash and relies on it to carry the reviews across, and moving a card
    -- between collections will change `collection_id` the same way.
    foreign key (collection_id, card_hash)
        references cards (collection_id, card_hash)
        on update cascade
        on delete cascade
) strict;

create table bookmarks (
    collection_id text not null,
    card_hash text not null,
    note text,
    created_at text not null,
    primary key (collection_id, card_hash),
    foreign key (collection_id, card_hash)
        references cards (collection_id, card_hash)
        on update cascade
        on delete cascade
) strict;

create index idx_reviews_card on reviews (collection_id, card_hash);
create index idx_reviews_session_id on reviews (session_id);
create index idx_reviews_reviewed_date on reviews (collection_id, reviewed_date);
create index idx_cards_due on cards (collection_id, due_date);

create table schema_version (
    version integer not null
) strict;

create table meta (
    key text primary key,
    value text not null
) strict;
