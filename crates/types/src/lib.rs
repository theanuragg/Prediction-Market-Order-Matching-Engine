use serde::{Deserialize, Serialize};

//        Required data model (fields may not be removed)                                                                               

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub id:    u64,
    pub side:  Side,
    pub price: u64, // integer ticks — no floats
    pub qty:   u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    pub maker_order_id: u64,
    pub taker_order_id: u64,
    pub price:          u64,
    pub qty:            u64,
}

//        Wire types for HTTP API                                                                                                                                                          

/// POST /orders  request body
#[derive(Debug, Deserialize)]
pub struct SubmitOrder {
    pub side:  Side,
    pub price: u64,
    pub qty:   u64,
}

/// POST /orders  response
#[derive(Debug, Serialize)]
pub struct OrderAck {
    pub order_id: u64,
}

/// GET /orderbook  response
#[derive(Debug, Serialize)]
pub struct OrderBook {
    /// price → remaining qty, sorted best-bid first (descending)
    pub bids: Vec<PriceLevel>,
    /// price → remaining qty, sorted best-ask first (ascending)
    pub asks: Vec<PriceLevel>,
}

#[derive(Debug, Serialize)]
pub struct PriceLevel {
    pub price: u64,
    pub qty:   u64,
}
