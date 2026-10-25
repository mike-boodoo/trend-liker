create table public_feeds (
    id          text primary key,
    name        text not null,
    description text not null default '',
    categories  text[] not null default '{}'
);

create table content_items (
    id          uuid primary key,
    source      text not null,
    external_id text,
    author      text not null,
    text        text not null,
    url         text,
    categories  text[] not null,
    created_at  timestamptz not null,
    score       double precision not null
);

-- Dedupe key for ingested content (local seed items have no external id).
create unique index content_items_source_external
    on content_items (source, external_id) where external_id is not null;

create table feed_items (
    feed_id text not null references public_feeds (id) on delete cascade,
    item_id uuid not null references content_items (id) on delete cascade,
    primary key (feed_id, item_id)
);
create index feed_items_item on feed_items (item_id);

create table users (
    user_id text primary key
);

create table user_subscriptions (
    user_id  text not null references users (user_id) on delete cascade,
    feed_id  text not null references public_feeds (id) on delete cascade,
    position bigint generated always as identity,
    primary key (user_id, feed_id)
);
