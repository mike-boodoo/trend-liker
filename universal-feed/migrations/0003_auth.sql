-- Sign in with X: link users to their X account.
alter table users add column x_id text unique;
alter table users add column username text;

-- Only a SHA-256 of each session token is stored.
create table sessions (
    token_hash bytea primary key,
    user_id    text not null references users (user_id) on delete cascade,
    created_at timestamptz not null default now(),
    expires_at timestamptz not null
);
create index sessions_user on sessions (user_id);
