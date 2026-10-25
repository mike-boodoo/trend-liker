use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use uuid::Uuid;
use async_trait::async_trait;
use crate::model::{Candidate, ContentItem, HomeFeed, IngestReport, PublicFeed, UserSettings};
use crate::repo::{Repo, Subscribed};

#[derive(Clone)]
pub struct Store {
    pub items: Arc<RwLock<HashMap<Uuid, ContentItem>>>,
    pub feeds: Arc<RwLock<HashMap<String, PublicFeed>>>,
    pub users: Arc<RwLock<HashMap<String, UserSettings>>>,
}

impl Store {
    pub fn new() -> Self {
        Self {
            items: Arc::new(RwLock::new(HashMap::new())),
            feeds: Arc::new(RwLock::new(HashMap::new())),
            users: Arc::new(RwLock::new(HashMap::new())),
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
    async fn home_feed(&self, user: &str) -> anyhow::Result<Option<HomeFeed>> {
        Ok(crate::feed::build_home_feed(self, user))
    }
    async fn ingest(&self, items: Vec<Candidate>) -> anyhow::Result<IngestReport> {
        Ok(crate::ingest::ingest(self, items))
    }
}
