use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use uuid::Uuid;
use crate::model::{ContentItem, PublicFeed, UserSettings};

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
