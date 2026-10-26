-- Publishing into shared feeds is opt-in per user; off until explicitly enabled.
alter table users add column publish_opt_in boolean not null default false;

-- Who contributed each ingested item, so withdrawing consent can remove it.
create table item_contributors (
    item_id uuid not null references content_items (id) on delete cascade,
    user_id text not null references users (user_id) on delete cascade,
    primary key (item_id, user_id)
);
create index item_contributors_user on item_contributors (user_id);
