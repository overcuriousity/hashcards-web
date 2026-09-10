-- Tokens that authenticate MCP clients.
--
-- Server-level rather than per user: a bearer token has to resolve to a
-- user before the user is known, so it cannot live in a file that is
-- chosen by knowing the user.
--
-- Only the digest is stored. The plaintext is shown once, when it is
-- minted, and never again -- so a stolen copy of this file yields nothing
-- that can be presented to the server.
create table if not exists tokens (
    token_hash   text primary key not null,
    owner        text,
    name         text not null,
    created_at   text not null,
    last_used_at text,
    revoked      integer not null default 0
);

create index if not exists tokens_owner on tokens (owner);
