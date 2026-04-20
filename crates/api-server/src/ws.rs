use axum::{
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    response::Response,
};
use futures_util::StreamExt;
use redis::Client;
use tracing::{error, info};

use crate::AppState;

const FILLS_CHANNEL: &str = "fills_channel";

pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    info!("ws client connected");

    // Each WS client needs its OWN dedicated Redis connection.
    // PubSub mode cannot share a connection with normal commands.
    let client = match Client::open(state.redis_url.clone()) {
        Ok(c)  => c,
        Err(e) => { error!("redis client open failed: {e}"); return; }
    };

    // redis 0.23 API: get_async_connection().into_pubsub()
    let conn = match client.get_async_connection().await {
        Ok(c)  => c,
        Err(e) => { error!("redis connection failed: {e}"); return; }
    };

    let mut pubsub = conn.into_pubsub();

    if let Err(e) = pubsub.subscribe(FILLS_CHANNEL).await {
        error!("subscribe failed: {e}");
        return;
    }

    info!("subscribed to fills_channel");

    let mut msg_stream = pubsub.on_message();

    loop {
        tokio::select! {
            // Redis PubSub → forward to WebSocket client.
            Some(redis_msg) = msg_stream.next() => {
                let payload: String = match redis_msg.get_payload() {
                    Ok(p)  => p,
                    Err(e) => { error!("payload error: {e}"); continue; }
                };
                if socket.send(Message::Text(payload.into())).await.is_err() {
                    break; // client disconnected
                }
            }
            // WebSocket frame from client — only care about close.
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }

    info!("ws client disconnected");
}
