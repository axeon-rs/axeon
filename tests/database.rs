use axeon::database::{Connection, ConnectionPool, DatabaseError, PoolConfig, PoolState};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Default)]
struct Counters {
    created: AtomicUsize,
    closed: AtomicUsize,
    fail_next: AtomicBool,
}

struct FakeConn {
    id: usize,
    valid: Arc<AtomicBool>,
    counters: Arc<Counters>,
}

impl Connection for FakeConn {
    fn is_valid(&self) -> bool {
        self.valid.load(Ordering::SeqCst)
    }
    fn close(&mut self) {
        self.counters.closed.fetch_add(1, Ordering::SeqCst);
    }
}

fn pool(config: PoolConfig) -> (ConnectionPool<FakeConn>, Arc<Counters>) {
    let counters = Arc::new(Counters::default());
    let c = Arc::clone(&counters);
    let pool = ConnectionPool::new(config, move || {
        if c.fail_next.swap(false, Ordering::SeqCst) {
            return Err(DatabaseError::ConnectionFailed);
        }
        Ok(FakeConn {
            id: c.created.fetch_add(1, Ordering::SeqCst),
            valid: Arc::new(AtomicBool::new(true)),
            counters: Arc::clone(&c),
        })
    });
    (pool, counters)
}

fn config(max_size: usize) -> PoolConfig {
    PoolConfig { max_size, min_idle: 0, ..PoolConfig::default() }
}

#[test]
fn min_idle_connections_are_opened_up_front() {
    let (pool, counters) = pool(PoolConfig { max_size: 5, min_idle: 2, ..PoolConfig::default() });
    assert_eq!(pool.state(), PoolState { size: 2, idle: 2 });
    assert_eq!(counters.created.load(Ordering::SeqCst), 2);
}

#[test]
fn try_get_respects_max_size_and_reuses_returned_connections() {
    let (pool, counters) = pool(config(2));
    let a = pool.try_get().unwrap();
    let b = pool.try_get().unwrap();
    assert_eq!(pool.try_get().err(), Some(DatabaseError::PoolExhausted));
    assert_eq!(pool.state(), PoolState { size: 2, idle: 0 });

    let a_id = a.id;
    drop(a);
    assert_eq!(pool.state(), PoolState { size: 2, idle: 1 });
    let again = pool.try_get().unwrap();
    assert_eq!(again.id, a_id, "returned connection is reused");
    assert_eq!(counters.created.load(Ordering::SeqCst), 2);
    drop((b, again));
    assert_eq!(pool.state(), PoolState { size: 2, idle: 2 });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_size_holds_under_concurrent_checkouts() {
    let (pool, counters) = pool(config(3));
    let out = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));

    let tasks: Vec<_> = (0..20)
        .map(|_| {
            let (pool, out, peak) = (pool.clone(), Arc::clone(&out), Arc::clone(&peak));
            tokio::spawn(async move {
                let conn = pool.get().await.unwrap();
                let now = out.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                assert!(pool.state().size <= 3);
                tokio::time::sleep(Duration::from_millis(10)).await;
                out.fetch_sub(1, Ordering::SeqCst);
                drop(conn);
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }

    assert!(peak.load(Ordering::SeqCst) <= 3);
    assert!(counters.created.load(Ordering::SeqCst) <= 3);
    assert_eq!(pool.state().size, counters.created.load(Ordering::SeqCst));
}

#[tokio::test]
async fn get_waits_for_a_returned_connection() {
    let (pool, _) = pool(PoolConfig { connection_timeout: Duration::from_secs(5), ..config(1) });
    let held = pool.get().await.unwrap();
    let held_id = held.id;
    let releaser = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(held);
    });
    let conn = pool.get().await.unwrap();
    assert_eq!(conn.id, held_id);
    releaser.await.unwrap();
}

#[tokio::test]
async fn get_times_out_when_exhausted() {
    let (pool, _) = pool(PoolConfig { connection_timeout: Duration::from_millis(50), ..config(1) });
    let _held = pool.get().await.unwrap();
    assert_eq!(pool.get().await.err(), Some(DatabaseError::Timeout));
}

#[test]
fn max_lifetime_is_enforced_across_reuse() {
    let (pool, counters) = pool(PoolConfig { max_lifetime: Duration::from_millis(300), ..config(2) });
    let first = pool.try_get().unwrap().id;
    // Reusing the connection must not reset its age.
    std::thread::sleep(Duration::from_millis(180));
    assert_eq!(pool.try_get().unwrap().id, first);
    std::thread::sleep(Duration::from_millis(180));

    let conn = pool.try_get().unwrap();
    assert_ne!(conn.id, first, "expired connection was reused");
    assert_eq!(counters.closed.load(Ordering::SeqCst), 1);
    assert_eq!(pool.state().size, 1);
}

#[test]
fn idle_timeout_closes_unused_connections() {
    let (pool, counters) = pool(PoolConfig { idle_timeout: Duration::from_millis(30), ..config(2) });
    let first = pool.try_get().unwrap().id;
    std::thread::sleep(Duration::from_millis(50));
    assert_ne!(pool.try_get().unwrap().id, first);
    assert_eq!(counters.closed.load(Ordering::SeqCst), 1);
}

#[test]
fn invalid_connections_are_discarded_on_return() {
    let (pool, counters) = pool(config(1));
    let conn = pool.try_get().unwrap();
    conn.valid.store(false, Ordering::SeqCst);
    drop(conn);
    assert_eq!(pool.state(), PoolState { size: 0, idle: 0 });
    assert_eq!(counters.closed.load(Ordering::SeqCst), 1);
    assert!(pool.try_get().is_ok());
}

#[tokio::test]
async fn failed_open_frees_its_slot() {
    let (pool, counters) = pool(config(1));
    counters.fail_next.store(true, Ordering::SeqCst);
    assert_eq!(pool.get().await.err(), Some(DatabaseError::ConnectionFailed));
    assert_eq!(pool.state().size, 0);
    assert!(pool.try_get().is_ok());
}

#[tokio::test]
async fn cancelled_get_frees_its_slot() {
    let counters = Arc::new(Counters::default());
    let c = Arc::clone(&counters);
    let pool = ConnectionPool::new(config(1), move || {
        std::thread::sleep(Duration::from_millis(100)); // slow connect
        Ok(FakeConn { id: 0, valid: Arc::new(AtomicBool::new(true)), counters: Arc::clone(&c) })
    });
    let cancelled = tokio::time::timeout(Duration::from_millis(10), pool.get()).await;
    assert!(cancelled.is_err());
    assert_eq!(pool.state().size, 0);
    assert!(pool.get().await.is_ok());
}
