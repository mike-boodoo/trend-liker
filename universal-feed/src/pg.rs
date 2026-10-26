//! Postgres-backed Repo (sqlx, runtime-checked queries, embedded migrations).

use std::collections::HashSet;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use sqlx::{postgres::PgPoolOptions, PgPool, Row};
use uuid::Uuid;

use crate::classify::classify;
use crate::feed::HOME_FEED_LIMIT;
use crate::ingest::{clamp_score, prepare_text};
use crate::model::*;
use crate::repo::{x_user_id, Ingested, OAuthClient, Repo, Subscribed, XTokens};

pub struct PgRepo {
    pool: PgPool,
}

impl PgRepo {
    /// Connect and apply migrations. `schema` isolates tables (used by tests).
    pub async fn connect(url: &str, schema: Option<&str>) -> Result<Self> {
        if let Some(s) = schema {
            anyhow::ensure!(s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'), "bad schema name");
            let admin = PgPoolOptions::new().max_connections(1).connect(url).await?;
            sqlx::query(&format!("create schema if not exists {s}")).execute(&admin).await?;
            admin.close().await;
        }
        let path = schema.map(|s| format!("set search_path to {s}"));
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .after_connect(move |conn, _| {
                let path = path.clone();
                Box::pin(async move {
                    if let Some(p) = path { sqlx::query(&p).execute(&mut *conn).await?; }
                    Ok(())
                })
            })
            .connect(url)
            .await
            .context("connecting to Postgres")?;
        sqlx::migrate!("./migrations").run(&pool).await.context("running migrations")?;
        Ok(Self { pool })
    }

    pub async fn close(&self) { self.pool.close().await; }

    /// Insert demo data only into an empty database.
    pub async fn seed_if_empty(&self) -> Result<bool> {
        let n: i64 = sqlx::query_scalar("select count(*) from public_feeds").fetch_one(&self.pool).await?;
        if n > 0 { return Ok(false); }
        let d = crate::seed::demo_data();
        let mut tx = self.pool.begin().await?;
        for f in &d.feeds {
            sqlx::query("insert into public_feeds (id, name, description, categories) values ($1,$2,$3,$4)")
                .bind(&f.id).bind(&f.name).bind(&f.description).bind(&f.categories)
                .execute(&mut *tx).await?;
        }
        for i in &d.items {
            insert_item(&mut tx, i).await?;
        }
        for f in &d.feeds {
            for item in &f.items {
                sqlx::query("insert into feed_items (feed_id, item_id) values ($1,$2)")
                    .bind(&f.id).bind(item).execute(&mut *tx).await?;
            }
        }
        for u in &d.users {
            sqlx::query("insert into users (user_id) values ($1)").bind(&u.user_id).execute(&mut *tx).await?;
            for feed in &u.public_feeds {
                sqlx::query("insert into user_subscriptions (user_id, feed_id) values ($1,$2)")
                    .bind(&u.user_id).bind(feed).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(true)
    }
}

async fn insert_item(tx: &mut sqlx::PgConnection, i: &ContentItem) -> Result<u64> {
    let r = sqlx::query(
        "insert into content_items (id, source, external_id, author, text, url, categories, created_at, score)
         values ($1,$2,$3,$4,$5,$6,$7,$8,$9)
         on conflict (source, external_id) where external_id is not null do nothing",
    )
    .bind(i.id).bind(i.source.as_str()).bind(&i.external_id).bind(&i.author).bind(&i.text)
    .bind(&i.url).bind(&i.categories).bind(i.created_at).bind(i.score)
    .execute(tx).await?;
    Ok(r.rows_affected())
}

fn row_to_item(r: &sqlx::postgres::PgRow) -> Result<ContentItem> {
    let source: String = r.try_get("source")?;
    Ok(ContentItem {
        id: r.try_get("id")?,
        source: Source::parse(&source).with_context(|| format!("unknown source {source:?} in database"))?,
        external_id: r.try_get("external_id")?,
        author: r.try_get("author")?,
        text: r.try_get("text")?,
        url: r.try_get("url")?,
        categories: r.try_get("categories")?,
        created_at: r.try_get("created_at")?,
        score: r.try_get("score")?,
    })
}

async fn subscriptions(pool: &PgPool, user: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar("select feed_id from user_subscriptions where user_id = $1 order by position")
        .bind(user).fetch_all(pool).await?)
}

#[async_trait]
impl Repo for PgRepo {
    async fn list_feeds(&self) -> Result<Vec<PublicFeed>> {
        let rows = sqlx::query(
            "select f.id, f.name, f.description, f.categories,
                    coalesce(array_agg(fi.item_id) filter (where fi.item_id is not null), '{}') as items
             from public_feeds f left join feed_items fi on fi.feed_id = f.id
             group by f.id order by f.id",
        ).fetch_all(&self.pool).await?;
        rows.iter().map(|r| Ok(PublicFeed {
            id: r.try_get("id")?, name: r.try_get("name")?, description: r.try_get("description")?,
            categories: r.try_get("categories")?, items: r.try_get::<Vec<Uuid>, _>("items")?,
        })).collect()
    }

    async fn get_user(&self, id: &str) -> Result<Option<UserSettings>> {
        let Some(row) = sqlx::query("select publish_opt_in, username from users where user_id = $1")
            .bind(id).fetch_optional(&self.pool).await? else { return Ok(None) };
        Ok(Some(UserSettings {
            user_id: id.to_string(),
            public_feeds: subscriptions(&self.pool, id).await?,
            publish_opt_in: row.try_get("publish_opt_in")?,
            username: row.try_get("username")?,
        }))
    }

    async fn upsert_x_user(&self, x_id: &str, username: &str) -> Result<String> {
        Ok(sqlx::query_scalar(
            "insert into users (user_id, x_id, username) values ($1, $2, $3)
             on conflict (user_id) do update set username = excluded.username
             returning user_id",
        ).bind(x_user_id(x_id)).bind(x_id).bind(username).fetch_one(&self.pool).await?)
    }

    async fn ensure_user(&self, user: &str) -> Result<()> {
        sqlx::query("insert into users (user_id) values ($1) on conflict do nothing").bind(user).execute(&self.pool).await?;
        Ok(())
    }

    async fn create_session(&self, token_hash: &[u8], user: &str, expires_at: chrono::DateTime<Utc>) -> Result<()> {
        sqlx::query("delete from sessions where user_id = $1 and expires_at <= now()").bind(user).execute(&self.pool).await?;
        sqlx::query("insert into sessions (token_hash, user_id, expires_at) values ($1, $2, $3)")
            .bind(token_hash).bind(user).bind(expires_at).execute(&self.pool).await?;
        Ok(())
    }

    async fn session_user(&self, token_hash: &[u8], now: chrono::DateTime<Utc>) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("select user_id from sessions where token_hash = $1 and expires_at > $2")
            .bind(token_hash).bind(now).fetch_optional(&self.pool).await?)
    }

    async fn delete_session(&self, token_hash: &[u8]) -> Result<()> {
        sqlx::query("delete from sessions where token_hash = $1").bind(token_hash).execute(&self.pool).await?;
        Ok(())
    }

    async fn put_x_tokens(&self, user: &str, t: &XTokens) -> Result<()> {
        sqlx::query(
            "insert into x_tokens (user_id, access_token, refresh_token, expires_at, scope) values ($1,$2,$3,$4,$5)
             on conflict (user_id) do update set access_token = excluded.access_token, refresh_token = excluded.refresh_token,
                 expires_at = excluded.expires_at, scope = excluded.scope, updated_at = now()",
        ).bind(user).bind(&t.access_token).bind(&t.refresh_token).bind(t.expires_at).bind(&t.scope).execute(&self.pool).await?;
        Ok(())
    }

    async fn get_x_tokens(&self, user: &str) -> Result<Option<XTokens>> {
        let Some(r) = sqlx::query("select access_token, refresh_token, expires_at, scope from x_tokens where user_id = $1")
            .bind(user).fetch_optional(&self.pool).await? else { return Ok(None) };
        Ok(Some(XTokens {
            access_token: r.try_get("access_token")?, refresh_token: r.try_get("refresh_token")?,
            expires_at: r.try_get("expires_at")?, scope: r.try_get("scope")?,
        }))
    }

    async fn delete_x_tokens(&self, user: &str) -> Result<bool> {
        Ok(sqlx::query("delete from x_tokens where user_id = $1").bind(user).execute(&self.pool).await?.rows_affected() > 0)
    }

    async fn register_client(&self, c: &OAuthClient) -> Result<()> {
        sqlx::query("insert into oauth_clients (client_id, client_name, redirect_uris) values ($1, $2, $3)")
            .bind(&c.client_id).bind(&c.client_name).bind(&c.redirect_uris).execute(&self.pool).await?;
        Ok(())
    }

    async fn get_client(&self, client_id: &str) -> Result<Option<OAuthClient>> {
        let Some(r) = sqlx::query("select client_id, client_name, redirect_uris from oauth_clients where client_id = $1")
            .bind(client_id).fetch_optional(&self.pool).await? else { return Ok(None) };
        Ok(Some(OAuthClient { client_id: r.try_get("client_id")?, client_name: r.try_get("client_name")?, redirect_uris: r.try_get("redirect_uris")? }))
    }

    async fn create_refresh_token(&self, token_hash: &[u8], client_id: &str, user: &str, expires_at: chrono::DateTime<Utc>) -> Result<()> {
        sqlx::query("delete from oauth_refresh_tokens where user_id = $1 and expires_at <= now()").bind(user).execute(&self.pool).await?;
        sqlx::query("insert into oauth_refresh_tokens (token_hash, client_id, user_id, expires_at) values ($1, $2, $3, $4)")
            .bind(token_hash).bind(client_id).bind(user).bind(expires_at).execute(&self.pool).await?;
        Ok(())
    }

    async fn take_refresh_token(&self, token_hash: &[u8], now: chrono::DateTime<Utc>) -> Result<Option<(String, String)>> {
        // `delete ... returning` makes the token single-use even under concurrent refreshes.
        let Some(r) = sqlx::query("delete from oauth_refresh_tokens where token_hash = $1 returning client_id, user_id, expires_at")
            .bind(token_hash).fetch_optional(&self.pool).await? else { return Ok(None) };
        let expires: chrono::DateTime<Utc> = r.try_get("expires_at")?;
        Ok((expires > now).then(|| Ok::<_, sqlx::Error>((r.try_get("client_id")?, r.try_get("user_id")?))).transpose()?)
    }

    async fn subscribe(&self, user: &str, feed: &str) -> Result<Subscribed> {
        let feed_ok: bool = sqlx::query_scalar("select exists (select 1 from public_feeds where id = $1)")
            .bind(feed).fetch_one(&self.pool).await?;
        if !feed_ok { return Ok(Subscribed::NoFeed); }
        let Some(_) = self.get_user(user).await? else { return Ok(Subscribed::NoUser) };
        sqlx::query("insert into user_subscriptions (user_id, feed_id) values ($1,$2) on conflict do nothing")
            .bind(user).bind(feed).execute(&self.pool).await?;
        Ok(match self.get_user(user).await? {
            Some(s) => Subscribed::Ok(s),
            None => Subscribed::NoUser,
        })
    }

    async fn home_feed(&self, user: &str) -> Result<Option<HomeFeed>> {
        let Some(settings) = self.get_user(user).await? else { return Ok(None) };
        let rows = sqlx::query(
            "select i.* from content_items i
             where i.id in (select fi.item_id from feed_items fi
                            join user_subscriptions s on s.feed_id = fi.feed_id
                            where s.user_id = $1)
             order by i.score desc, i.created_at desc, i.id
             limit $2",
        ).bind(user).bind(HOME_FEED_LIMIT as i64).fetch_all(&self.pool).await?;
        let items = rows.iter().map(row_to_item).collect::<Result<Vec<_>>>()?;
        Ok(Some(HomeFeed { user_id: user.to_string(), feeds: settings.public_feeds, items }))
    }

    async fn set_publishing(&self, user: &str, enabled: bool) -> Result<Option<PublishingSetting>> {
        let mut tx = self.pool.begin().await?;
        // The row lock taken here is what serializes this against in-flight ingests (FOR SHARE).
        let updated = sqlx::query("update users set publish_opt_in = $2 where user_id = $1")
            .bind(user).bind(enabled).execute(&mut *tx).await?.rows_affected();
        if updated == 0 { return Ok(None); }
        let mut withdrawn = 0;
        if !enabled {
            // Lock the user's items first so a concurrent co-contribution can't be cascaded away.
            sqlx::query("select id from content_items
                         where id in (select item_id from item_contributors where user_id = $1) for update")
                .bind(user).fetch_all(&mut *tx).await?;
            // The snapshot still contains this user's rows, hence `user_id <> $1`.
            withdrawn = sqlx::query(
                "with removed as (delete from item_contributors where user_id = $1 returning item_id)
                 delete from content_items ci using removed r
                 where ci.id = r.item_id
                   and not exists (select 1 from item_contributors c where c.item_id = ci.id and c.user_id <> $1)",
            ).bind(user).execute(&mut *tx).await?.rows_affected() as usize;
        }
        tx.commit().await?;
        Ok(Some(PublishingSetting { user_id: user.to_string(), enabled, withdrawn }))
    }

    async fn ingest(&self, user: &str, candidates: Vec<Candidate>) -> Result<Ingested> {
        let mut report = IngestReport::default();
        let mut tx = self.pool.begin().await?;
        match sqlx::query_scalar::<_, bool>("select publish_opt_in from users where user_id = $1 for share")
            .bind(user).fetch_optional(&mut *tx).await?
        {
            None => return Ok(Ingested::NoUser),
            Some(false) => return Ok(Ingested::NotOptedIn),
            Some(true) => {}
        }
        let allowed: HashSet<String> = sqlx::query_scalar("select id from public_feeds")
            .fetch_all(&mut *tx).await?.into_iter().collect();

        for c in candidates {
            let Some(text) = prepare_text(&c) else { report.rejected += 1; continue };
            if let Some(id) = find_item(&mut tx, c.source, &c.external_id).await? {
                // Already published (possibly by someone else): record this user as a co-contributor.
                add_contributor(&mut tx, id, user).await?;
                report.duplicates += 1;
                continue;
            }
            let categories = classify(&text, &|cat| allowed.contains(cat));
            if categories.is_empty() { report.unclassified += 1; continue; }

            let item = ContentItem {
                id: Uuid::new_v4(), source: c.source, external_id: Some(c.external_id.clone()),
                author: c.author.clone(), text, url: c.url.clone(), categories: categories.clone(),
                created_at: Utc::now(), score: clamp_score(&c),
            };
            // A concurrent ingest may win the race; the unique index makes that a duplicate.
            if insert_item(&mut tx, &item).await? == 0 {
                if let Some(id) = find_item(&mut tx, c.source, &c.external_id).await? {
                    add_contributor(&mut tx, id, user).await?;
                }
                report.duplicates += 1;
                continue;
            }
            add_contributor(&mut tx, item.id, user).await?;
            for cat in &categories {
                sqlx::query("insert into feed_items (feed_id, item_id) values ($1,$2)")
                    .bind(cat).bind(item.id).execute(&mut *tx).await?;
            }
            report.ingested.push(IngestedItem { external_id: c.external_id, id: item.id, categories });
        }
        tx.commit().await?;
        Ok(Ingested::Report(report))
    }
}

/// `for key share` makes this wait out a concurrent withdrawal instead of racing it.
async fn find_item(tx: &mut sqlx::PgConnection, source: Source, external_id: &str) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar("select id from content_items where source = $1 and external_id = $2 for key share")
        .bind(source.as_str()).bind(external_id).fetch_optional(tx).await?)
}

async fn add_contributor(tx: &mut sqlx::PgConnection, item: Uuid, user: &str) -> Result<()> {
    sqlx::query("insert into item_contributors (item_id, user_id) values ($1,$2) on conflict do nothing")
        .bind(item).bind(user).execute(tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    /// None (test skipped) unless TEST_DATABASE_URL is set.
    async fn fresh() -> Option<PgRepo> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        let schema = format!("t_{}", Uuid::new_v4().simple());
        let repo = PgRepo::connect(&url, Some(&schema)).await.unwrap();
        assert!(repo.seed_if_empty().await.unwrap());
        Some(repo)
    }

    /// Fresh database with "demo" and a second user "bob" both opted in.
    async fn fresh_opted_in() -> Option<PgRepo> {
        let pg = fresh().await?;
        sqlx::query("insert into users (user_id) values ('bob')").execute(&pg.pool).await.unwrap();
        for u in ["demo", "bob"] { pg.set_publishing(u, true).await.unwrap().unwrap(); }
        Some(pg)
    }

    fn cand(source: Source, ext: &str, text: &str, score: Option<f64>) -> Candidate {
        Candidate { source, external_id: ext.into(), author: "a".into(), text: text.into(), url: None, score }
    }

    async fn ingest(pg: &PgRepo, user: &str, items: Vec<Candidate>) -> IngestReport {
        match pg.ingest(user, items).await.unwrap() { Ingested::Report(r) => r, _ => panic!("expected a report") }
    }

    async fn count(pg: &PgRepo, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(&pg.pool).await.unwrap()
    }

    #[tokio::test]
    async fn seed_is_idempotent_and_matches_demo_shape() {
        let Some(pg) = fresh().await else { return eprintln!("skipped: TEST_DATABASE_URL unset") };
        assert!(!pg.seed_if_empty().await.unwrap());
        let feeds = pg.list_feeds().await.unwrap();
        assert_eq!(feeds.len(), 8);
        let ai = feeds.iter().find(|f| f.id == "ai").unwrap();
        assert_eq!(ai.items.len(), 3);
        let home = pg.home_feed("demo").await.unwrap().unwrap();
        assert_eq!(home.feeds, vec!["ai", "rust", "cloud"]);
        assert_eq!(home.items.len(), 6);
        assert_eq!(home.items[0].score, 0.98);
        assert!(!pg.get_user("demo").await.unwrap().unwrap().publish_opt_in, "publishing is opt-in");
    }

    #[tokio::test]
    async fn subscribe_semantics() {
        let Some(pg) = fresh().await else { return };
        assert!(matches!(pg.subscribe("nobody", "ai").await.unwrap(), Subscribed::NoUser));
        assert!(matches!(pg.subscribe("demo", "nope").await.unwrap(), Subscribed::NoFeed));
        assert!(matches!(pg.subscribe("nobody", "nope").await.unwrap(), Subscribed::NoFeed)); // feed checked first
        let Subscribed::Ok(s) = pg.subscribe("demo", "agents").await.unwrap() else { panic!() };
        assert_eq!(s.public_feeds, vec!["ai", "rust", "cloud", "agents"]);
        let Subscribed::Ok(s) = pg.subscribe("demo", "agents").await.unwrap() else { panic!() };
        assert_eq!(s.public_feeds.len(), 4); // idempotent, order preserved
        assert!(pg.home_feed("nobody").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn ingest_dedupes_classifies_and_persists() {
        let Some(pg) = fresh_opted_in().await else { return };
        let r = ingest(&pg, "demo", vec![
            cand(Source::X, "1", "Tokio and axum deep dive", Some(0.99)),
            cand(Source::X, "1", "Tokio and axum deep dive", Some(0.99)),
            cand(Source::X, "2", "my lunch", None),
            cand(Source::X, "", "rust", None),
            cand(Source::X, "3", "rust", Some(7.0)),
        ]).await;
        assert_eq!((r.ingested.len(), r.duplicates, r.unclassified, r.rejected), (2, 1, 1, 1));
        assert_eq!(ingest(&pg, "demo", vec![cand(Source::X, "1", "rust", None)]).await.duplicates, 1);
        assert_eq!(ingest(&pg, "demo", vec![cand(Source::TikTok, "1", "rust", None)]).await.ingested.len(), 1);

        let home = pg.home_feed("demo").await.unwrap().unwrap();
        assert_eq!(home.items[0].external_id.as_deref(), Some("3")); // clamped to 1.0, outranks 0.99
        assert_eq!(home.items[0].score, 1.0);
        assert!(home.items.iter().any(|i| i.source == Source::TikTok));
    }

    #[tokio::test]
    async fn ingest_requires_opt_in_and_known_user() {
        let Some(pg) = fresh().await else { return };
        assert!(matches!(pg.ingest("demo", vec![cand(Source::X, "1", "rust", None)]).await.unwrap(), Ingested::NotOptedIn));
        assert!(matches!(pg.ingest("ghost", vec![cand(Source::X, "1", "rust", None)]).await.unwrap(), Ingested::NoUser));
        assert_eq!(count(&pg, "select count(*) from content_items").await, 6, "nothing stored without consent");
        assert!(pg.set_publishing("ghost", true).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn opting_out_withdraws_only_unshared_contributions() {
        let Some(pg) = fresh_opted_in().await else { return };
        ingest(&pg, "demo", vec![cand(Source::X, "solo", "rust solo", None), cand(Source::X, "shared", "rust shared", None)]).await;
        ingest(&pg, "bob", vec![cand(Source::X, "shared", "rust shared", None)]).await; // co-contributes
        assert_eq!(count(&pg, "select count(*) from item_contributors").await, 3);

        let out = pg.set_publishing("demo", false).await.unwrap().unwrap();
        assert_eq!((out.enabled, out.withdrawn), (false, 1));
        assert_eq!(count(&pg, "select count(*) from content_items where external_id = 'solo'").await, 0);
        assert_eq!(count(&pg, "select count(*) from content_items where external_id = 'shared'").await, 1);
        assert_eq!(count(&pg, "select count(*) from content_items").await, 7, "seed items untouched");
        assert_eq!(count(&pg, "select count(*) from feed_items fi left join content_items i on i.id = fi.item_id where i.id is null").await, 0);
        assert!(matches!(pg.ingest("demo", vec![cand(Source::X, "again", "rust", None)]).await.unwrap(), Ingested::NotOptedIn));

        assert_eq!(pg.set_publishing("bob", false).await.unwrap().unwrap().withdrawn, 1); // last contributor
        assert_eq!(count(&pg, "select count(*) from content_items where source = 'X'").await, 0);
        // Re-enabling does not resurrect anything.
        assert_eq!(pg.set_publishing("demo", true).await.unwrap().unwrap().withdrawn, 0);
    }

    #[tokio::test]
    async fn concurrent_ingest_of_same_post_stores_it_once() {
        let Some(pg) = fresh_opted_in().await else { return };
        let pg = std::sync::Arc::new(pg);
        let tasks: Vec<_> = (0..8).map(|n| {
            let pg = pg.clone();
            let user = if n % 2 == 0 { "demo" } else { "bob" };
            tokio::spawn(async move { ingest(&pg, user, vec![cand(Source::X, "race", "rust news", None)]).await })
        }).collect();
        let mut ingested = 0;
        for t in tasks { ingested += t.await.unwrap().ingested.len(); }
        assert_eq!(ingested, 1);
        assert_eq!(count(&pg, "select count(*) from content_items where external_id = 'race'").await, 1);
        assert_eq!(count(&pg, "select count(*) from item_contributors").await, 2, "both users recorded");
    }

    #[tokio::test]
    async fn opt_out_racing_with_ingest_never_leaves_orphans() {
        let Some(pg) = fresh_opted_in().await else { return };
        let pg = std::sync::Arc::new(pg);
        for round in 0..10 {
            let (a, b) = (pg.clone(), pg.clone());
            let ext = format!("r{round}");
            let ing = tokio::spawn(async move { a.ingest("demo", vec![cand(Source::X, &ext, "rust race", None)]).await.unwrap() });
            let out = tokio::spawn(async move { b.set_publishing("demo", false).await.unwrap() });
            ing.await.unwrap(); out.await.unwrap();
            // Whatever the interleaving: no X item may exist unless an opted-in user contributed it.
            assert_eq!(count(&pg, "select count(*) from content_items i where source = 'X'
                                   and not exists (select 1 from item_contributors c where c.item_id = i.id)").await, 0);
            let opted = pg.get_user("demo").await.unwrap().unwrap().publish_opt_in;
            assert!(!opted);
            // Anything still stored must have been ingested before the opt-out committed... which removes it.
            assert_eq!(count(&pg, "select count(*) from content_items where source = 'X'").await, 0);
            pg.set_publishing("demo", true).await.unwrap();
        }
    }

    #[tokio::test]
    async fn x_users_and_sessions() {
        let Some(pg) = fresh().await else { return };
        assert_eq!(pg.upsert_x_user("42", "mike").await.unwrap(), "x-42");
        assert_eq!(pg.upsert_x_user("42", "mike_renamed").await.unwrap(), "x-42");
        let u = pg.get_user("x-42").await.unwrap().unwrap();
        assert_eq!((u.username.as_deref(), u.publish_opt_in, u.public_feeds.len()), (Some("mike_renamed"), false, 0));

        let now = Utc::now();
        pg.create_session(b"live", "x-42", now + chrono::Duration::hours(1)).await.unwrap();
        pg.create_session(b"stale", "x-42", now - chrono::Duration::seconds(1)).await.unwrap();
        assert_eq!(pg.session_user(b"live", now).await.unwrap().as_deref(), Some("x-42"));
        assert_eq!(pg.session_user(b"stale", now).await.unwrap(), None, "expired");
        assert_eq!(pg.session_user(b"nope", now).await.unwrap(), None);
        pg.delete_session(b"live").await.unwrap();
        assert_eq!(pg.session_user(b"live", now).await.unwrap(), None);

        pg.create_session(b"again", "x-42", now + chrono::Duration::hours(1)).await.unwrap();
        assert_eq!(count(&pg, "select count(*) from sessions where user_id = 'x-42'").await, 1, "expired ones pruned");
        sqlx::query("delete from users where user_id = 'x-42'").execute(&pg.pool).await.unwrap();
        assert_eq!(pg.session_user(b"again", now).await.unwrap(), None, "sessions go with the user");

        pg.ensure_user("dev").await.unwrap();
        pg.ensure_user("dev").await.unwrap();
        assert!(pg.get_user("dev").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn connector_storage() {
        let Some(pg) = fresh().await else { return };
        pg.upsert_x_user("7", "zed").await.unwrap();
        let t = XTokens { access_token: vec![1, 2], refresh_token: Some(vec![3]), expires_at: Utc::now(), scope: "tweet.read".into() };
        pg.put_x_tokens("x-7", &t).await.unwrap();
        let t2 = XTokens { access_token: vec![9], refresh_token: None, ..t.clone() };
        pg.put_x_tokens("x-7", &t2).await.unwrap();
        let got = pg.get_x_tokens("x-7").await.unwrap().unwrap();
        assert_eq!((got.access_token, got.refresh_token), (vec![9], None), "replaced, not duplicated");
        assert!(pg.delete_x_tokens("x-7").await.unwrap());
        assert!(!pg.delete_x_tokens("x-7").await.unwrap());

        let c = OAuthClient { client_id: "c1".into(), client_name: "Claude".into(), redirect_uris: vec!["https://a/cb".into()] };
        pg.register_client(&c).await.unwrap();
        assert_eq!(pg.get_client("c1").await.unwrap(), Some(c));
        assert_eq!(pg.get_client("nope").await.unwrap(), None);

        let now = Utc::now();
        pg.create_refresh_token(b"rt", "c1", "x-7", now + chrono::Duration::days(1)).await.unwrap();
        pg.create_refresh_token(b"old", "c1", "x-7", now - chrono::Duration::seconds(1)).await.unwrap();
        assert_eq!(pg.take_refresh_token(b"old", now).await.unwrap(), None, "expired");
        let pg = std::sync::Arc::new(pg);
        let takes: Vec<_> = (0..8).map(|_| { let pg = pg.clone(); tokio::spawn(async move { pg.take_refresh_token(b"rt", Utc::now()).await.unwrap() }) }).collect();
        let mut won = 0;
        for t in takes { won += t.await.unwrap().is_some() as usize; }
        assert_eq!(won, 1, "a refresh token works once, even under concurrent use");
        sqlx::query("delete from users where user_id = 'x-7'").execute(&pg.pool).await.unwrap();
        assert_eq!(count(&pg, "select count(*) from oauth_refresh_tokens").await, 0, "removed with the user");
    }

    #[tokio::test]
    async fn parity_with_in_memory_store() {
        let Some(pg) = fresh_opted_in().await else { return };
        let mem = Store::new();
        crate::seed::seed(&mem);
        mem.insert_user(UserSettings { user_id: "bob".into(), public_feeds: vec![], publish_opt_in: true, username: None });
        Repo::set_publishing(&mem, "demo", true).await.unwrap();
        let batch = || vec![
            cand(Source::X, "a", "Agents with MCP #AI", Some(0.7)),
            cand(Source::X, "a", "dup", None),
            cand(Source::X, "b", "k8s helm charts", None),
            cand(Source::X, "c", "nothing relevant", None),
            cand(Source::X, " ", "rust", None),
        ];
        let rm = match Repo::ingest(&mem, "demo", batch()).await.unwrap() { Ingested::Report(r) => r, _ => panic!() };
        let rp = ingest(&pg, "demo", batch()).await;
        assert_eq!((rm.ingested.len(), rm.duplicates, rm.unclassified, rm.rejected),
                   (rp.ingested.len(), rp.duplicates, rp.unclassified, rp.rejected));
        // bob co-contributes "a"; demo then withdraws -> "b" goes, "a" stays, in both backends.
        let bob = || vec![cand(Source::X, "a", "Agents with MCP #AI", None)];
        Repo::ingest(&mem, "bob", bob()).await.unwrap();
        pg.ingest("bob", bob()).await.unwrap();
        let wm = Repo::set_publishing(&mem, "demo", false).await.unwrap();
        let wp = pg.set_publishing("demo", false).await.unwrap();
        assert_eq!(wm, wp);
        assert_eq!(wp.unwrap().withdrawn, 1);
        for user in ["demo", "nobody"] {
            let (hm, hp) = (Repo::home_feed(&mem, user).await.unwrap(), pg.home_feed(user).await.unwrap());
            let texts = |h: Option<HomeFeed>| h.map(|h| h.items.iter().map(|i| (i.text.clone(), i.score)).collect::<Vec<_>>());
            assert_eq!(texts(hm), texts(hp));
        }
        let cats = |v: Vec<PublicFeed>| v.into_iter().map(|f| (f.id, f.items.len())).collect::<Vec<_>>();
        assert_eq!(cats(Repo::list_feeds(&mem).await.unwrap()), cats(pg.list_feeds().await.unwrap()));
    }
}
