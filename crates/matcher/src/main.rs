use anyhow::{Context, Result};
use matching::Book;
use redis::{AsyncCommands, Client};
use tracing::{error, info, warn};
use types::{Order, Side};

const STREAM_KEY:    &str = "orders_stream";
const FILLS_CHANNEL: &str = "fills_channel";
const BIDS_KEY:      &str = "orderbook:bids";
const ASKS_KEY:      &str = "orderbook:asks";
const GROUP_NAME:    &str = "matching_workers";
const CONSUMER_NAME: &str = "worker-0";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("matcher=info".parse().unwrap()),
        )
        .init();

    let redis_url = std::env::var("REDIS_URL")
        .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());

    info!("connecting to Redis at {}", redis_url);

    let client = Client::open(redis_url.clone()).context("bad redis URL")?;

    let mut cmd_conn = client
        .get_multiplexed_tokio_connection()
        .await
        .context("cmd connection failed")?;

    let mut pub_conn = client
        .get_multiplexed_tokio_connection()
        .await
        .context("pub connection failed")?;

    // Create consumer group. MKSTREAM creates the stream if absent.
    // BUSYGROUP error means group already exists — ignore it.
    let _: redis::RedisResult<()> = redis::cmd("XGROUP")
        .arg("CREATE")
        .arg(STREAM_KEY)
        .arg(GROUP_NAME)
        .arg("0")
        .arg("MKSTREAM")
        .query_async(&mut cmd_conn)
        .await;

    info!("consumer group ready — reclaiming any pending messages");
    reclaim_pending(&mut cmd_conn).await;

    let mut book = Book::new();

    info!("entering match loop");
    loop {
        // BLOCK 0 = wait forever until a new entry arrives.
        // ">" = only deliver messages not yet delivered to any consumer.
        let raw: redis::Value = redis::cmd("XREADGROUP")
            .arg("GROUP").arg(GROUP_NAME)
            .arg(CONSUMER_NAME)
            .arg("COUNT").arg(1)
            .arg("BLOCK").arg(0)
            .arg("STREAMS").arg(STREAM_KEY).arg(">")
            .query_async(&mut cmd_conn)
            .await
            .context("XREADGROUP error")?;

        let (entry_id, order) = match parse_entry(&raw) {
            Some(v) => v,
            None    => { warn!("could not parse stream entry — skipping"); continue; }
        };

        info!(order_id = order.id, side = ?order.side, price = order.price, qty = order.qty, "processing");

        // Pure in-memory match — zero I/O inside this call.
        let fills = book.process(order);
        info!(fills = fills.len(), "matched");

        // Publish each fill to all subscribed API server instances.
        for fill in &fills {
            let payload = serde_json::to_string(fill).unwrap();
            let _: redis::RedisResult<i64> = pub_conn.publish(FILLS_CHANNEL, &payload).await;
        }

        // Rewrite the orderbook snapshot so GET /orderbook stays accurate.
        if let Err(e) = update_snapshot(&mut cmd_conn, &book).await {
            error!("snapshot update failed: {e}");
        }

        // XACK only after fills published + snapshot written.
        // Crash before this → XAUTOCLAIM redelivers on restart (safe to replay).
        let _: redis::RedisResult<i64> = redis::cmd("XACK")
            .arg(STREAM_KEY)
            .arg(GROUP_NAME)
            .arg(&entry_id)
            .query_async(&mut cmd_conn)
            .await;
    }
}

async fn reclaim_pending(conn: &mut redis::aio::MultiplexedConnection) {
    let min_idle_ms: u64 = std::env::var("CLAIM_MIN_IDLE_MS")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(5_000);
    let _: redis::RedisResult<redis::Value> = redis::cmd("XAUTOCLAIM")
        .arg(STREAM_KEY).arg(GROUP_NAME).arg(CONSUMER_NAME)
        .arg(min_idle_ms).arg("0-0")
        .query_async(conn).await;
}

async fn update_snapshot(
    conn: &mut redis::aio::MultiplexedConnection,
    book: &Book,
) -> Result<()> {
    let bids = book.bids_snapshot();
    let asks = book.asks_snapshot();

    let mut pipe = redis::pipe();
    pipe.atomic();
    pipe.cmd("DEL").arg(BIDS_KEY).ignore();
    pipe.cmd("DEL").arg(ASKS_KEY).ignore();

    for (price, qty) in &bids {
        pipe.hset(BIDS_KEY, price.to_string(), qty.to_string()).ignore();
    }
    for (price, qty) in &asks {
        pipe.hset(ASKS_KEY, price.to_string(), qty.to_string()).ignore();
    }

    pipe.query_async(conn).await.context("snapshot pipeline failed")
}



fn parse_entry(val: &redis::Value) -> Option<(String, Order)> {
    use redis::Value::{Bulk};


    let outer   = match val   { redis::Value::Bulk(a) => a.first()?,   _ => return None };
    let pair    = match outer { redis::Value::Bulk(a) => a,             _ => return None };
    let entries = match pair.get(1)? { Bulk(a) => a,     _ => return None };
    let entry   = match entries.first()? { Bulk(a) => a, _ => return None };

    let entry_id = val_to_str(entry.first()?)?;
    let fields   = match entry.get(1)? { Bulk(a) => a,   _ => return None };

    let mut id:    Option<u64>  = None;
    let mut side:  Option<Side> = None;
    let mut price: Option<u64>  = None;
    let mut qty:   Option<u64>  = None;

    let mut i = 0usize;
    while i + 1 < fields.len() {
        if let (Some(k), Some(v)) = (val_to_str(&fields[i]), val_to_str(&fields[i + 1])) {
            match k.as_str() {
                "id"    => id    = v.parse().ok(),
                "side"  => side  = parse_side(&v),
                "price" => price = v.parse().ok(),
                "qty"   => qty   = v.parse().ok(),
                _       => {}
            }
        }
        i += 2;
    }

    // Silence unused-variant warning — Status is a valid variant we handle in val_to_str

    Some((entry_id, Order { id: id?, side: side?, price: price?, qty: qty? }))
}

fn val_to_str(val: &redis::Value) -> Option<String> {
    match val {
        redis::Value::Data(b)   => String::from_utf8(b.clone()).ok(),
        redis::Value::Status(s) => Some(s.clone()),
        _                       => None,
    }
}

fn parse_side(s: &str) -> Option<Side> {
    match s { "buy" => Some(Side::Buy), "sell" => Some(Side::Sell), _ => None }
}
