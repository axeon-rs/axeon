mod plugin;

use std::sync::{Arc, Mutex};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub use plugin::DatabasePlugin;

#[derive(Debug)]
pub enum DatabaseError {
    PoolExhausted,
    ConnectionFailed,
    InvalidConnection,
}

pub trait Connection: Send + Sync {
    fn is_valid(&self) -> bool;
    fn close(&mut self);
}

pub struct PoolConfig {
    pub max_size: usize,
    pub min_idle: usize,
    pub max_lifetime: Duration,
    pub idle_timeout: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_size: 10,
            min_idle: 2,
            max_lifetime: Duration::from_secs(30 * 60), // 30 minutes
            idle_timeout: Duration::from_secs(10 * 60), // 10 minutes
        }
    }
}

struct PooledConnection<C: Connection> {
    connection: C,
    created_at: Instant,
    last_used_at: Instant,
}

pub struct ConnectionPool<C: Connection> {
    connections: Arc<Mutex<VecDeque<PooledConnection<C>>>>,
    config: PoolConfig,
    create_connection: Arc<dyn Fn() -> Result<C, DatabaseError> + Send + Sync>,
}

impl<C: Connection + 'static> ConnectionPool<C> {
    pub fn new<F>(config: PoolConfig, create_fn: F) -> Self 
    where 
        F: Fn() -> Result<C, DatabaseError> + Send + Sync + 'static 
    {
        let connections = Arc::new(Mutex::new(VecDeque::with_capacity(config.max_size)));
        let pool = Self {
            connections: connections.clone(),
            config,
            create_connection: Arc::new(create_fn),
        };

        // Initialize minimum idle connections
        {
            let mut guard = connections.lock().unwrap();
            for _ in 0..pool.config.min_idle {
                if let Ok(conn) = pool.create_new_connection() {
                    guard.push_back(conn);
                }
            }
        }

        pool
    }

    pub fn get(&self) -> Result<C, DatabaseError> {
        let mut connections = self.connections.lock().unwrap();
        let now = Instant::now();

        // Remove expired connections
        while let Some(pooled) = connections.front() {
            if now.duration_since(pooled.created_at) > self.config.max_lifetime
                || now.duration_since(pooled.last_used_at) > self.config.idle_timeout {
                let mut expired = connections.pop_front().unwrap();
                expired.connection.close();
                continue;
            }
            break;
        }

        // Try to get an existing connection
        if let Some(mut pooled) = connections.pop_front() {
            if pooled.connection.is_valid() {
                pooled.last_used_at = now;
                return Ok(pooled.connection);
            }
            pooled.connection.close();
        }

        // Create new connection if under max_size
        if connections.len() < self.config.max_size {
            if let Ok(conn) = self.create_new_connection() {
                return Ok(conn.connection);
            }
        }

        Err(DatabaseError::PoolExhausted)
    }

    pub fn release(&self, connection: C) {
        let mut connections = self.connections.lock().unwrap();
        if connections.len() < self.config.max_size && connection.is_valid() {
            connections.push_back(PooledConnection {
                connection,
                created_at: Instant::now(),
                last_used_at: Instant::now(),
            });
        } else {
            // Close connection if pool is full or connection is invalid
            let mut conn = connection;
            conn.close();
        }
    }

    fn create_new_connection(&self) -> Result<PooledConnection<C>, DatabaseError> {
        let connection = (self.create_connection)()?;
        if !connection.is_valid() {
            return Err(DatabaseError::InvalidConnection);
        }

        Ok(PooledConnection {
            connection,
            created_at: Instant::now(),
            last_used_at: Instant::now(),
        })
    }
}