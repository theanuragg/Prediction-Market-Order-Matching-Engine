use axum::{extract::State, Json};
use redis::AsyncCommands;
use types::{OrderBook, PriceLevel};

use crate::AppState;

const BIDS_KEY: &str = "orderbook:bids";
const ASKS_KEY: &str = "orderbook:asks";

pub async fn get_orderbook(State(state): State<AppState>) -> Json<OrderBook> {
    let mut conn = state.redis.lock().await;

    let bids_raw: Vec<(String, String)> = conn.hgetall(BIDS_KEY).await.unwrap_or_default();
    let asks_raw: Vec<(String, String)> = conn.hgetall(ASKS_KEY).await.unwrap_or_default();

    let mut bids: Vec<PriceLevel> = bids_raw.into_iter()
        .filter_map(|(p, q)| Some(PriceLevel { price: p.parse().ok()?, qty: q.parse().ok()? }))
        .filter(|l| l.qty > 0)
        .collect();

    let mut asks: Vec<PriceLevel> = asks_raw.into_iter()
        .filter_map(|(p, q)| Some(PriceLevel { price: p.parse().ok()?, qty: q.parse().ok()? }))
        .filter(|l| l.qty > 0)
        .collect();

    // Bids: highest price first. Asks: lowest price first.
    bids.sort_unstable_by(|a, b| b.price.cmp(&a.price));
    asks.sort_unstable_by(|a, b| a.price.cmp(&b.price));

    Json(OrderBook { bids, asks })
}
