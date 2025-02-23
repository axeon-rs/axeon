use std::sync::Arc;
use crate::database::{Connection, ConnectionPool, PoolConfig, DatabaseError};

pub struct DatabasePlugin<C: Connection> {
    pool: Arc<ConnectionPool<C>>,
}

impl<C: Connection + 'static> DatabasePlugin<C> {
    pub fn new<F>(config: PoolConfig, create_fn: F) -> Self 
    where 
        F: Fn() -> Result<C, DatabaseError> + Send + Sync + 'static 
    {
        Self {
            pool: Arc::new(ConnectionPool::new(config, create_fn)),
        }
    }

    pub fn get_connection(&self) -> Result<C, DatabaseError> {
        self.pool.get()
    }

    pub fn release_connection(&self, connection: C) {
        self.pool.release(connection);
    }
}

impl<C: Connection> Clone for DatabasePlugin<C> {
    fn clone(&self) -> Self {
        Self {
            pool: Arc::clone(&self.pool),
        }
    }
}