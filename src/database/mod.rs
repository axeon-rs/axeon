mod plugin;

use std::collections::VecDeque;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub use plugin::DatabasePlugin;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DatabaseError {
    /// No connection is free and the pool is at `max_size` (from `try_get`).
    PoolExhausted,
    /// No connection became free within `connection_timeout` (from `get`).
    Timeout,
    ConnectionFailed,
    InvalidConnection,
}

impl fmt::Display for DatabaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DatabaseError::PoolExhausted => write!(f, "connection pool exhausted"),
            DatabaseError::Timeout => write!(f, "timed out waiting for a pooled connection"),
            DatabaseError::ConnectionFailed => write!(f, "failed to create connection"),
            DatabaseError::InvalidConnection => write!(f, "connection is not valid"),
        }
    }
}

impl std::error::Error for DatabaseError {}

pub trait Connection: Send + Sync {
    fn is_valid(&self) -> bool;
    fn close(&mut self);
}

#[derive(Clone, Debug)]
pub struct PoolConfig {
    /// Maximum number of live connections, idle and checked out together.
    pub max_size: usize,
    /// Connections opened when the pool is created.
    pub min_idle: usize,
    /// Connections older than this are closed instead of reused.
    pub max_lifetime: Duration,
    /// Connections unused for this long are closed instead of reused.
    pub idle_timeout: Duration,
    /// How long [`ConnectionPool::get`] waits for a free connection.
    pub connection_timeout: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_size: 10,
            min_idle: 2,
            max_lifetime: Duration::from_secs(30 * 60), // 30 minutes
            idle_timeout: Duration::from_secs(10 * 60), // 10 minutes
            connection_timeout: Duration::from_secs(30),
        }
    }
}

/// Counts reported by [`ConnectionPool::state`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolState {
    /// Live connections: idle, checked out, and being opened.
    pub size: usize,
    pub idle: usize,
}

struct Entry<C> {
    connection: C,
    created_at: Instant,
    last_used_at: Instant,
}

struct State<C> {
    idle: VecDeque<Entry<C>>,
    /// Live connections, including checked-out ones and ones being opened.
    total: usize,
}

struct Shared<C> {
    state: Mutex<State<C>>,
    /// One permit per checked-out connection, so at most `max_size` are out.
    checkout: Arc<Semaphore>,
    config: PoolConfig,
    create_connection: Arc<dyn Fn() -> Result<C, DatabaseError> + Send + Sync>,
}

/// A bounded pool of connections.
///
/// Connections are handed out as [`PooledConn`] guards that return the
/// connection to the pool when dropped. Clones share the same pool.
pub struct ConnectionPool<C: Connection> {
    shared: Arc<Shared<C>>,
}

impl<C: Connection> Clone for ConnectionPool<C> {
    fn clone(&self) -> Self {
        Self { shared: Arc::clone(&self.shared) }
    }
}

impl<C: Connection + 'static> ConnectionPool<C> {
    /// Creates the pool and opens `min_idle` connections (blocking).
    pub fn new<F>(config: PoolConfig, create_fn: F) -> Self
    where
        F: Fn() -> Result<C, DatabaseError> + Send + Sync + 'static,
    {
        let max_size = config.max_size.max(1);
        let min_idle = config.min_idle.min(max_size);
        let pool = Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State { idle: VecDeque::with_capacity(max_size), total: 0 }),
                checkout: Arc::new(Semaphore::new(max_size)),
                config,
                create_connection: Arc::new(create_fn),
            }),
        };

        for _ in 0..min_idle {
            if let Ok(connection) = Shared::open(&pool.shared.create_connection) {
                let now = Instant::now();
                let mut state = pool.shared.lock();
                state.total += 1;
                state.idle.push_back(Entry { connection, created_at: now, last_used_at: now });
            }
        }

        pool
    }

    /// Gets a connection, waiting up to `connection_timeout` for one to be
    /// returned when the pool is at `max_size`. New connections are opened
    /// on Tokio's blocking thread pool.
    pub async fn get(&self) -> Result<PooledConn<C>, DatabaseError> {
        let permit = tokio::time::timeout(
            self.shared.config.connection_timeout,
            Arc::clone(&self.shared.checkout).acquire_owned(),
        )
        .await
        .map_err(|_| DatabaseError::Timeout)?
        .expect("pool semaphore is never closed");

        if let Some(entry) = self.shared.take_idle() {
            return Ok(self.guard(entry, permit));
        }
        let reservation = self.shared.reserve();
        let create = Arc::clone(&self.shared.create_connection);
        let opened = tokio::task::spawn_blocking(move || Shared::open(&create))
            .await
            .unwrap_or(Err(DatabaseError::ConnectionFailed));
        self.finish_open(opened, reservation, permit)
    }

    /// Gets a connection without waiting. Returns `PoolExhausted` when all
    /// `max_size` connections are checked out. May block while opening a
    /// new connection.
    pub fn try_get(&self) -> Result<PooledConn<C>, DatabaseError> {
        let permit = Arc::clone(&self.shared.checkout)
            .try_acquire_owned()
            .map_err(|_| DatabaseError::PoolExhausted)?;

        if let Some(entry) = self.shared.take_idle() {
            return Ok(self.guard(entry, permit));
        }
        let reservation = self.shared.reserve();
        let opened = Shared::open(&self.shared.create_connection);
        self.finish_open(opened, reservation, permit)
    }

    pub fn state(&self) -> PoolState {
        let state = self.shared.lock();
        PoolState { size: state.total, idle: state.idle.len() }
    }

    fn finish_open(
        &self,
        opened: Result<C, DatabaseError>,
        reservation: Reservation<'_, C>,
        permit: OwnedSemaphorePermit,
    ) -> Result<PooledConn<C>, DatabaseError> {
        // On error the reservation is dropped, freeing the slot.
        let connection = opened?;
        reservation.keep();
        let now = Instant::now();
        Ok(self.guard(Entry { connection, created_at: now, last_used_at: now }, permit))
    }

    fn guard(&self, entry: Entry<C>, permit: OwnedSemaphorePermit) -> PooledConn<C> {
        PooledConn { entry: Some(entry), shared: Arc::clone(&self.shared), _permit: permit }
    }
}

impl<C: Connection> Shared<C> {
    fn lock(&self) -> MutexGuard<'_, State<C>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn expired(&self, entry: &Entry<C>, now: Instant) -> bool {
        now.duration_since(entry.created_at) > self.config.max_lifetime
            || now.duration_since(entry.last_used_at) > self.config.idle_timeout
    }

    /// Pops the most recently used idle connection that is still usable.
    /// Stale connections are closed outside the lock.
    fn take_idle(&self) -> Option<Entry<C>> {
        let now = Instant::now();
        let mut stale = Vec::new();
        let found = {
            let mut state = self.lock();
            let mut found = None;
            while let Some(entry) = state.idle.pop_back() {
                if self.expired(&entry, now) || !entry.connection.is_valid() {
                    stale.push(entry);
                } else {
                    found = Some(entry);
                    break;
                }
            }
            state.total -= stale.len();
            found
        };
        for mut entry in stale {
            entry.connection.close();
        }
        found
    }

    /// Counts a connection that is about to be opened. Only called while
    /// holding a checkout permit with no idle connections, so `total` stays
    /// within `max_size`.
    fn reserve(&self) -> Reservation<'_, C> {
        self.lock().total += 1;
        Reservation { shared: self, kept: false }
    }

    fn open(create: &Arc<dyn Fn() -> Result<C, DatabaseError> + Send + Sync>) -> Result<C, DatabaseError> {
        let mut connection = create()?;
        if !connection.is_valid() {
            connection.close();
            return Err(DatabaseError::InvalidConnection);
        }
        Ok(connection)
    }

    fn put_back(&self, mut entry: Entry<C>) {
        let now = Instant::now();
        // Keep `created_at` so `max_lifetime` still applies.
        if entry.connection.is_valid() && now.duration_since(entry.created_at) <= self.config.max_lifetime {
            entry.last_used_at = now;
            self.lock().idle.push_back(entry);
        } else {
            self.lock().total -= 1;
            entry.connection.close();
        }
    }
}

/// A slot counted in `total` for a connection being opened. Released on drop
/// unless kept, so failed, panicking or cancelled opens don't leak capacity.
struct Reservation<'a, C> {
    shared: &'a Shared<C>,
    kept: bool,
}

impl<C> Reservation<'_, C> {
    fn keep(mut self) {
        self.kept = true;
    }
}

impl<C> Drop for Reservation<'_, C> {
    fn drop(&mut self) {
        if !self.kept {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.total -= 1;
        }
    }
}

/// A checked-out connection. Dereferences to the connection and returns it
/// to the pool when dropped.
pub struct PooledConn<C: Connection> {
    entry: Option<Entry<C>>,
    shared: Arc<Shared<C>>,
    // Dropped after `Drop::drop` has returned the connection.
    _permit: OwnedSemaphorePermit,
}

impl<C: Connection> Deref for PooledConn<C> {
    type Target = C;

    fn deref(&self) -> &C {
        &self.entry.as_ref().expect("connection present until drop").connection
    }
}

impl<C: Connection> DerefMut for PooledConn<C> {
    fn deref_mut(&mut self) -> &mut C {
        &mut self.entry.as_mut().expect("connection present until drop").connection
    }
}

impl<C: Connection> Drop for PooledConn<C> {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            self.shared.put_back(entry);
        }
    }
}
