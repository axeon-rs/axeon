use moka::future::Cache;
use std::hash::Hash;
use std::time::Duration;

pub struct CacheManager<K, V>
where
    K: Clone + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    cache: Cache<K, V>,
}

impl<K, V> CacheManager<K, V> 
where 
    K: Clone + Eq + Send + Sync + Hash + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn new(max_capacity: u64, ttl: Duration) -> Self {
        let cache = Cache::builder()
            .max_capacity(max_capacity)
            .time_to_live(ttl)
            .build();
        
        Self { cache }
    }

    pub async fn get(&self, key: K) -> Option<V> where K: Hash  {
        self.cache.get(&key).await
    }

    pub async fn set(&self, key: K, value: V) where K: Hash {
        self.cache.insert(key, value).await;
    }

    pub async fn remove(&self, key: &K) where K: Hash {
        self.cache.remove(key).await;
    }

    pub async fn clear(&self) {
        self.cache.invalidate_all();
    }
}

// Helper type for JSON caching
pub type JsonCache<K> = CacheManager<K, serde_json::Value>;

// Default configuration
pub fn default_cache<K, V>(max_capacity: u64) -> CacheManager<K, V> 
where 
    K: Clone + Eq + Send + Sync + Hash + 'static,
    V: Clone + Send + Sync + 'static,
{
    CacheManager::new(max_capacity, Duration::from_secs(300)) // 5 minutes default TTL
}