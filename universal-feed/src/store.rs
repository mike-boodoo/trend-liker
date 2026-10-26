use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use uuid::Uuid;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use crate::model::{Candidate, ContentItem, HomeFeed, PublicFeed, PublishingSetting, UserSettings};
use crate::repo::{x_user_id, Ingested, Repo, Subscribed};

#[derive(Clone)]
pub struct Store {
    pub items: Arc<RwLock<HashMap<Uuid, ContentItem>>>,
    pub feeds: Arc<RwLock<HashMap<String, PublicFeed>>>,
    pub users: Arc<RwLock<HashMap<String, UserSettings>>>,
    /// item -> users who contributed it. Lock order everywhere: feeds, items, contributors, users.
    pub contributors: Arc<RwLock<HashMap<Uuid, HashSet<String>>>>,
    /// token hash -> (user, expiry)
    pub sessions: Arc<RwLock<HashMap<Vec<u8>, (String, DateTime<Utc>)>>>,
}

impl Store {
    pub fn new() -> Self {
        Self {
            items: Arc::new(RwLock::new(HashMap::new())),
            feeds: Arc::new(RwLock::new(HashMap::new())),
            users: Arc::new(RwLock::new(HashMap::new())),
            contributors: Arc::new(RwLock::new(HashMap::new())),
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    pub fn insert_item(&self, item: ContentItem) { self.items.write().unwrap().insert(item.id, item); }
    pub fn insert_feed(&self, feed: PublicFeed) { self.feeds.write().unwrap().insert(feed.id.clone(), feed); }
    pub fn insert_user(&self, user: UserSettings) { self.users.write().unwrap().insert(user.user_id.clone(), user); }
    pub fn get_user(&self, id: &str) -> Option<UserSettings> { self.users.read().unwrap().get(id).cloned() }
}

#[async_trait]
impl Repo for Store {
    async fn list_feeds(&self) -> anyhow::Result<Vec<PublicFeed>> {
        let mut v: Vec<_> = self.feeds.read().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(v)
    }
    async fn get_user(&self, id: &str) -> anyhow::Result<Option<UserSettings>> {
        Ok(Store::get_user(self, id))
    }
    async fn subscribe(&self, user: &str, feed: &str) -> anyhow::Result<Subscribed> {
        if !self.feeds.read().unwrap().contains_key(feed) { return Ok(Subscribed::NoFeed); }
        let mut users = self.users.write().unwrap();
        let Some(settings) = users.get_mut(user) else { return Ok(Subscribed::NoUser) };
        if !settings.public_feeds.iter().any(|f| f == feed) { settings.public_feeds.push(feed.to_string()); }
        Ok(Subscribed::Ok(settings.clone()))
    }
    async fn set_publishing(&self, user: &str, enabled: bool) -> anyhow::Result<Option<PublishingSetting>> {
        let mut feeds = self.feeds.write().unwrap();
        let mut items = self.items.write().unwrap();
        let mut contributors = self.contributors.write().unwrap();
        let mut users = self.users.write().unwrap();
        let Some(settings) = users.get_mut(user) else { return Ok(None) };
        settings.publish_opt_in = enabled;
        let mut withdrawn = 0;
        if !enabled {
            let orphaned: Vec<Uuid> = contributors.iter_mut()
                .filter_map(|(id, who)| (who.remove(user) && who.is_empty()).then_some(*id))
                .collect();
            for id in &orphaned {
                contributors.remove(id);
                items.remove(id);
                for feed in feeds.values_mut() { feed.items.retain(|i| i != id); }
            }
            withdrawn = orphaned.len();
        }
        Ok(Some(PublishingSetting { user_id: user.to_string(), enabled, withdrawn }))
    }
    async fn home_feed(&self, user: &str) -> anyhow::Result<Option<HomeFeed>> {
        Ok(crate::feed::build_home_feed(self, user))
    }
    async fn ingest(&self, user: &str, items: Vec<Candidate>) -> anyhow::Result<Ingested> {
        Ok(crate::ingest::ingest(self, user, items))
    }
    async fn upsert_x_user(&self, x_id: &str, username: &str) -> anyhow::Result<String> {
        let id = x_user_id(x_id);
        let mut users = self.users.write().unwrap();
        let u = users.entry(id.clone()).or_insert_with(|| UserSettings {
            user_id: id.clone(), public_feeds: vec![], publish_opt_in: false, username: None,
        });
        u.username = Some(username.to_string());
        Ok(id)
    }
    async fn ensure_user(&self, user: &str) -> anyhow::Result<()> {
        self.users.write().unwrap().entry(user.to_string()).or_insert_with(|| UserSettings {
            user_id: user.to_string(), public_feeds: vec![], publish_opt_in: false, username: None,
        });
        Ok(())
    }
    async fn create_session(&self, token_hash: &[u8], user: &str, expires_at: DateTime<Utc>) -> anyhow::Result<()> {
        let mut sessions = self.sessions.write().unwrap();
        let now = Utc::now();
        sessions.retain(|_, (_, exp)| *exp > now);
        sessions.insert(token_hash.to_vec(), (user.to_string(), expires_at));
        Ok(())
    }
    async fn session_user(&self, token_hash: &[u8], now: DateTime<Utc>) -> anyhow::Result<Option<String>> {
        let sessions = self.sessions.read().unwrap();
        let user = sessions.get(token_hash).filter(|(_, exp)| *exp > now).map(|(u, _)| u.clone());
        // A session outlives its user only if the user was removed; treat that as signed out.
        Ok(user.filter(|u| self.users.read().unwrap().contains_key(u)))
    }
    async fn delete_session(&self, token_hash: &[u8]) -> anyhow::Result<()> {
        self.sessions.write().unwrap().remove(token_hash);
        Ok(())
    }
}
