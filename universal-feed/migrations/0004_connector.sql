-- X tokens, so the server can read a user's own home timeline for them.
-- Encrypted with AES-256-GCM (nonce || ciphertext), bound to user_id; the key is not in the database.
create table x_tokens (
    user_id       text primary key references users (user_id) on delete cascade,
    access_token  bytea not null,
    refresh_token bytea,
    expires_at    timestamptz not null,
    scope         text not null default '',
    updated_at    timestamptz not null default now()
);

-- OAuth clients that registered themselves (e.g. claude.ai adding this as a connector).
create table oauth_clients (
    client_id     text primary key,
    client_name   text not null default '',
    redirect_uris text[] not null,
    created_at    timestamptz not null default now()
);

-- Refresh tokens issued to those clients. Single-use: each refresh rotates it. Hash only.
create table oauth_refresh_tokens (
    token_hash bytea primary key,
    client_id  text not null references oauth_clients (client_id) on delete cascade,
    user_id    text not null references users (user_id) on delete cascade,
    expires_at timestamptz not null
);
create index oauth_refresh_tokens_user on oauth_refresh_tokens (user_id);
