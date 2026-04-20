use axum::{extract::State, http::StatusCode, Json};
use redis::AsyncCommands;
use serde::Serialize;
use tracing::{error, info};
use types::{OrderAck, Side, SubmitOrder};

use crate::AppState;

const STREAM_KEY: &str = "orders_stream";
const ID_KEY:     &str = "order_id_seq";

#[derive(Serialize)]
pub struct ErrorBody {
    pub error: String,
}

pub async fn submit_order(
    State(state): State<AppState>,
    Json(body): Json<SubmitOrder>,
) -> Result<Json<OrderAck>, (StatusCode, Json<ErrorBody>)> {
    if body.price == 0 {
        return Err((StatusCode::UNPROCESSABLE_ENTITY,
            Json(ErrorBody { error: "price must be > 0".into() })));
    }
    if body.qty == 0 {
        return Err((StatusCode::UNPROCESSABLE_ENTITY,
            Json(ErrorBody { error: "qty must be > 0".into() })));
    }

    let mut conn = state.redis.lock().await;

    // INCR is atomic in Redis — unique across all API instances.
    let order_id: u64 = conn.incr(ID_KEY, 1u64).await.map_err(|e| {
        error!("INCR failed: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorBody { error: "redis error".into() }))
    })?;

    let side_str = match body.side { Side::Buy => "buy", Side::Sell => "sell" };

    // XADD with auto-generated stream entry ID ("*").
    let _: String = redis::cmd("XADD")
        .arg(STREAM_KEY).arg("*")
        .arg("id")    .arg(order_id)
        .arg("side")  .arg(side_str)
        .arg("price") .arg(body.price)
        .arg("qty")   .arg(body.qty)
        .query_async(&mut *conn)
        .await
        .map_err(|e| {
            error!("XADD failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorBody { error: "redis error".into() }))
        })?;

    info!(order_id, side = side_str, price = body.price, qty = body.qty, "enqueued");
    Ok(Json(OrderAck { order_id }))
}
