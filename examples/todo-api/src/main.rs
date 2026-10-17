//! Run with:
//!
//! ```sh
//! JWT_SECRET=$(openssl rand -hex 32) ADMIN_PASSWORD=change-me cargo run
//! curl -X POST localhost:3000/auth/token -H 'content-type: application/json' \
//!      -d '{"username":"admin","password":"change-me"}'
//! curl localhost:3000/todos -H "authorization: Bearer <token>"
//! ```

use std::time::Duration;
use todo_api::{app, connect, Config};

#[tokio::main]
async fn main() {
    // Settings: environment variables, optionally from a .env file.
    axeon::config::load_dotenv().expect("invalid .env file");
    let config: Config = axeon::config::from_env().unwrap_or_else(|error| {
        eprintln!("configuration error: {}", error);
        std::process::exit(1);
    });
    tracing_subscriber::fmt().with_target(false).init();

    let pool = connect(&config.database_url).await.expect("database connection failed");
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.port)).await.expect("can't bind port");

    let mut server = app(pool, config);
    // On SIGTERM: report not-ready for 5s so load balancers move away, then
    // finish in-flight requests (up to 30s).
    server.shutdown_delay(Duration::from_secs(5)).shutdown_timeout(Duration::from_secs(30));
    server.serve_with_shutdown(listener, axeon::shutdown_signal()).await.expect("server failed");
}
