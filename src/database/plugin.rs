use crate::database::{Connection, ConnectionPool, DatabaseError, PoolConfig, PoolState, PooledConn};

pub struct DatabasePlugin<C: Connection> {
    pool: ConnectionPool<C>,
}

impl<C: Connection + 'static> DatabasePlugin<C> {
    pub fn new<F>(config: PoolConfig, create_fn: F) -> Self
    where
        F: Fn() -> Result<C, DatabaseError> + Send + Sync + 'static,
    {
        Self {
            pool: ConnectionPool::new(config, create_fn),
        }
    }

    /// Waits up to `connection_timeout` for a connection. The connection
    /// returns to the pool when the guard is dropped.
    pub async fn get_connection(&self) -> Result<PooledConn<C>, DatabaseError> {
        self.pool.get().await
    }

    /// Gets a connection without waiting for one to be returned.
    pub fn try_get_connection(&self) -> Result<PooledConn<C>, DatabaseError> {
        self.pool.try_get()
    }

    pub fn state(&self) -> PoolState {
        self.pool.state()
    }
}

impl<C: Connection> Clone for DatabasePlugin<C> {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
        }
    }
}
