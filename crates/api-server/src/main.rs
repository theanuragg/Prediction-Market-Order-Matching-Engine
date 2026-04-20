mod routes;
mod ws;

use anyhow::Result;
use axum::{routing::{get, post}, Router};
use redis::{aio::MultiplexedConnection, Client};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

/// Shared state cloned cheaply into every handler.
/// MultiplexedConnection is NOT Clone, so we wrap in Arc<Mutex<>>.
/// Axum clones AppState per-request; the Arc ensures the same underlying
/// connection is shared without re-connecting on every request.
#[derive(Clone)]
pub struct AppState {
    pub redis:     Arc<Mutex<MultiplexedConnection>>,
    pub redis_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("api_server=info".parse().unwrap()),
        )
        .init();

    let redis_url = std::env::var("REDIS_URL")
        .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let port: u16 = std::env::var("PORT")
        .ok().and_then(|p| p.parse().ok()).unwrap_or(8080);

    info!("connecting to Redis at {}", redis_url);
    let client = Client::open(redis_url.clone())?;
    let conn = client.get_multiplexed_tokio_connection().await?;

    let state = AppState {
        redis:     Arc::new(Mutex::new(conn)),
        redis_url,
    };

    let app = Router::new()
        .route("/orders",    post(routes::orders::submit_order))
        .route("/orderbook", get(routes::orderbook::get_orderbook))
        .route("/ws/fills",  get(ws::ws_handler))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    info!("listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
