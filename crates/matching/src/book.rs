use std::cmp::Reverse;
use std::collections::{BTreeMap, VecDeque};
use types::{Fill, Order, Side};

/// Central limit order book.
///
/// Bids: `BTreeMap<Reverse<u64>, VecDeque<Order>>`
///   – `Reverse` makes BTreeMap iterate highest price first (best bid first).
///   – `VecDeque` gives O(1) push_back (new order) and pop_front (oldest = time priority).
///
/// Asks: `BTreeMap<u64, VecDeque<Order>>`
///   – Natural ascending order gives lowest ask first (best ask first).
pub struct Book {
    bids: BTreeMap<Reverse<u64>, VecDeque<Order>>,
    asks: BTreeMap<u64, VecDeque<Order>>,
}

impl Default for Book {
    fn default() -> Self {
        Self::new()
    }
}

impl Book {
    pub fn new() -> Self {
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
        }
    }

    /// Process one incoming order. Returns all fills generated.
    /// Any unfilled remainder is placed on the resting book.
    pub fn process(&mut self, mut incoming: Order) -> Vec<Fill> {
        match incoming.side {
            Side::Buy  => self.match_buy(&mut incoming),
            Side::Sell => self.match_sell(&mut incoming),
        }
    }

    /// Snapshot of bids: (price, total_qty) sorted descending (best bid first).
    pub fn bids_snapshot(&self) -> Vec<(u64, u64)> {
        self.bids
            .iter()
            .map(|(Reverse(price), queue)| {
                let total: u64 = queue.iter().map(|o| o.qty).sum();
                (*price, total)
            })
            .filter(|(_, qty)| *qty > 0)
            .collect()
    }

    /// Snapshot of asks: (price, total_qty) sorted ascending (best ask first).
    pub fn asks_snapshot(&self) -> Vec<(u64, u64)> {
        self.asks
            .iter()
            .map(|(price, queue)| {
                let total: u64 = queue.iter().map(|o| o.qty).sum();
                (*price, total)
            })
            .filter(|(_, qty)| *qty > 0)
            .collect()
    }

    // ── BUY matching ─────────────────────────────────────────────────────────

    fn match_buy(&mut self, incoming: &mut Order) -> Vec<Fill> {
        let mut fills = Vec::new();

        loop {
            if incoming.qty == 0 {
                break;
            }

            // Best ask = entry with the lowest price key (natural BTreeMap order).
            let best_ask_price = match self.asks.keys().next().copied() {
                Some(p) => p,
                None    => break, // no asks resting — incoming buy rests
            };

            // Match condition: taker buy price must be >= maker ask price.
            if incoming.price < best_ask_price {
                break;
            }

            let queue = self.asks.get_mut(&best_ask_price).unwrap();
            let maker = queue.front_mut().unwrap();

            let fill_qty = incoming.qty.min(maker.qty);

            // Fill price is always the MAKER's price.
            fills.push(Fill {
                maker_order_id: maker.id,
                taker_order_id: incoming.id,
                price:          best_ask_price,
                qty:            fill_qty,
            });

            maker.qty    -= fill_qty;
            incoming.qty -= fill_qty;

            // Remove maker if fully consumed.
            if maker.qty == 0 {
                queue.pop_front();
                if queue.is_empty() {
                    self.asks.remove(&best_ask_price);
                }
            }
        }

        // Remaining incoming qty rests on the bid side.
        if incoming.qty > 0 {
            self.bids
                .entry(Reverse(incoming.price))
                .or_default()
                .push_back(incoming.clone());
        }

        fills
    }

    // ── SELL matching ────────────────────────────────────────────────────────

    fn match_sell(&mut self, incoming: &mut Order) -> Vec<Fill> {
        let mut fills = Vec::new();

        loop {
            if incoming.qty == 0 {
                break;
            }

            // Best bid = entry with highest price (Reverse makes first key = highest).
            let best_bid_price = match self.bids.keys().next().map(|Reverse(p)| *p) {
                Some(p) => p,
                None    => break, // no bids resting — incoming sell rests
            };

            // Match condition: taker sell price must be <= maker bid price.
            if incoming.price > best_bid_price {
                break;
            }

            let queue = self.bids.get_mut(&Reverse(best_bid_price)).unwrap();
            let maker = queue.front_mut().unwrap();

            let fill_qty = incoming.qty.min(maker.qty);

            fills.push(Fill {
                maker_order_id: maker.id,
                taker_order_id: incoming.id,
                price:          best_bid_price,
                qty:            fill_qty,
            });

            maker.qty    -= fill_qty;
            incoming.qty -= fill_qty;

            if maker.qty == 0 {
                queue.pop_front();
                if queue.is_empty() {
                    self.bids.remove(&Reverse(best_bid_price));
                }
            }
        }

        // Remaining incoming qty rests on the ask side.
        if incoming.qty > 0 {
            self.asks
                .entry(incoming.price)
                .or_default()
                .push_back(incoming.clone());
        }

        fills
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use types::Side;

    fn buy(id: u64, price: u64, qty: u64) -> Order {
        Order { id, side: Side::Buy, price, qty }
    }
    fn sell(id: u64, price: u64, qty: u64) -> Order {
        Order { id, side: Side::Sell, price, qty }
    }

    // ── 1. No match — buy below ask ──────────────────────────────────────────
    #[test]
    fn no_match_buy_below_ask() {
        let mut book = Book::new();
        assert!(book.process(sell(1, 100, 10)).is_empty());
        assert!(book.process(buy(2, 99, 5)).is_empty());
        assert_eq!(book.asks_snapshot(), vec![(100, 10)]);
        assert_eq!(book.bids_snapshot(), vec![(99, 5)]);
    }

    // ── 2. No match — sell above bid ─────────────────────────────────────────
    #[test]
    fn no_match_sell_above_bid() {
        let mut book = Book::new();
        assert!(book.process(buy(1, 100, 10)).is_empty());
        assert!(book.process(sell(2, 101, 5)).is_empty());
        assert_eq!(book.bids_snapshot(), vec![(100, 10)]);
        assert_eq!(book.asks_snapshot(), vec![(101, 5)]);
    }

    // ── 3. Exact fill — buy side taker ───────────────────────────────────────
    #[test]
    fn exact_fill_buy_taker() {
        let mut book = Book::new();
        book.process(sell(1, 100, 10));
        let fills = book.process(buy(2, 100, 10));
        assert_eq!(fills.len(), 1);
        let f = &fills[0];
        assert_eq!(f.maker_order_id, 1);
        assert_eq!(f.taker_order_id, 2);
        assert_eq!(f.price, 100);
        assert_eq!(f.qty, 10);
        assert!(book.bids_snapshot().is_empty());
        assert!(book.asks_snapshot().is_empty());
    }

    // ── 4. Exact fill — sell side taker ──────────────────────────────────────
    #[test]
    fn exact_fill_sell_taker() {
        let mut book = Book::new();
        book.process(buy(1, 100, 8));
        let fills = book.process(sell(2, 100, 8));
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].maker_order_id, 1);
        assert_eq!(fills[0].taker_order_id, 2);
        assert_eq!(fills[0].price, 100);
        assert_eq!(fills[0].qty, 8);
        assert!(book.bids_snapshot().is_empty());
        assert!(book.asks_snapshot().is_empty());
    }

    // ── 5. Partial fill — taker smaller than maker ───────────────────────────
    #[test]
    fn partial_fill_taker_smaller() {
        let mut book = Book::new();
        book.process(sell(1, 100, 10));
        let fills = book.process(buy(2, 100, 6));
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].qty, 6);
        // Maker has 4 remaining on the ask side.
        assert_eq!(book.asks_snapshot(), vec![(100, 4)]);
        assert!(book.bids_snapshot().is_empty());
    }

    // ── 6. Partial fill — taker larger than maker ────────────────────────────
    #[test]
    fn partial_fill_taker_larger() {
        let mut book = Book::new();
        book.process(sell(1, 100, 6));
        let fills = book.process(buy(2, 100, 10));
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].qty, 6);
        // Buyer's remainder (4) rests on bid side.
        assert!(book.asks_snapshot().is_empty());
        assert_eq!(book.bids_snapshot(), vec![(100, 4)]);
    }

    // ── 7. Sweep multiple ask levels ─────────────────────────────────────────
    #[test]
    fn sweep_multiple_ask_levels() {
        let mut book = Book::new();
        book.process(sell(1, 100, 5));
        book.process(sell(2, 101, 3));
        book.process(sell(3, 102, 8));
        // Buy 12 @ 102: fills @ 100 (5), @ 101 (3), @ 102 (4 partial).
        let fills = book.process(buy(4, 102, 12));
        assert_eq!(fills.len(), 3);
        assert_eq!((fills[0].price, fills[0].qty), (100, 5));
        assert_eq!((fills[1].price, fills[1].qty), (101, 3));
        assert_eq!((fills[2].price, fills[2].qty), (102, 4));
        // 4 remaining at 102.
        assert_eq!(book.asks_snapshot(), vec![(102, 4)]);
        assert!(book.bids_snapshot().is_empty());
    }

    // ── 8. Sweep multiple bid levels ─────────────────────────────────────────
    #[test]
    fn sweep_multiple_bid_levels() {
        let mut book = Book::new();
        book.process(buy(1, 102, 5));
        book.process(buy(2, 101, 3));
        book.process(buy(3, 100, 8));
        // Sell 12 @ 100: fills @ 102 (5), @ 101 (3), @ 100 (4 partial).
        let fills = book.process(sell(4, 100, 12));
        assert_eq!(fills.len(), 3);
        assert_eq!((fills[0].price, fills[0].qty), (102, 5));
        assert_eq!((fills[1].price, fills[1].qty), (101, 3));
        assert_eq!((fills[2].price, fills[2].qty), (100, 4));
        assert_eq!(book.bids_snapshot(), vec![(100, 4)]);
        assert!(book.asks_snapshot().is_empty());
    }

    // ── 9. Time priority — same price, earlier order fills first ─────────────
    #[test]
    fn time_priority_asks() {
        let mut book = Book::new();
        // id=1 arrives first, id=2 second — both @ 100.
        book.process(sell(1, 100, 5));
        book.process(sell(2, 100, 5));
        let fills = book.process(buy(3, 100, 5));
        assert_eq!(fills.len(), 1);
        // id=1 should be the maker (arrived first).
        assert_eq!(fills[0].maker_order_id, 1);
        // id=2 still resting.
        assert_eq!(book.asks_snapshot(), vec![(100, 5)]);
    }

    // ── 10. Time priority — same bid price ───────────────────────────────────
    #[test]
    fn time_priority_bids() {
        let mut book = Book::new();
        book.process(buy(1, 100, 5));
        book.process(buy(2, 100, 5));
        let fills = book.process(sell(3, 100, 5));
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].maker_order_id, 1);
        assert_eq!(book.bids_snapshot(), vec![(100, 5)]);
    }

    // ── 11. Price priority — lower ask fills before higher ask ───────────────
    #[test]
    fn price_priority_asks() {
        let mut book = Book::new();
        // Insert out of price order — lower ask must still win.
        book.process(sell(1, 102, 5));
        book.process(sell(2, 100, 5)); // lower price, should match first
        book.process(sell(3, 101, 5));
        let fills = book.process(buy(4, 103, 6));
        // First fill @ 100 (5), second fill @ 101 (1).
        assert_eq!(fills[0].price, 100);
        assert_eq!(fills[0].qty, 5);
        assert_eq!(fills[1].price, 101);
        assert_eq!(fills[1].qty, 1);
    }

    // ── 12. Fill price is always maker price ─────────────────────────────────
    #[test]
    fn fill_price_is_maker_price() {
        let mut book = Book::new();
        book.process(sell(1, 100, 10));
        // Buy @ 110 — aggressive price, but fill is at maker's 100.
        let fills = book.process(buy(2, 110, 5));
        assert_eq!(fills[0].price, 100);
    }

    // ── 13. No self-cross (order rests when book empty) ──────────────────────
    #[test]
    fn rests_on_empty_book() {
        let mut book = Book::new();
        let fills = book.process(buy(1, 100, 5));
        assert!(fills.is_empty());
        assert_eq!(book.bids_snapshot(), vec![(100, 5)]);
        assert!(book.asks_snapshot().is_empty());
    }

    // ── 14. Multiple orders same side, all rest ──────────────────────────────
    #[test]
    fn multiple_bids_aggregate_correctly() {
        let mut book = Book::new();
        book.process(buy(1, 100, 3));
        book.process(buy(2, 100, 7)); // same price level
        book.process(buy(3, 99, 5));  // different level
        let bids = book.bids_snapshot();
        // 100 → 10, 99 → 5, descending.
        assert_eq!(bids, vec![(100, 10), (99, 5)]);
    }

    // ── 15. Full book clear then new orders work correctly ───────────────────
    #[test]
    fn book_empties_and_accepts_new_orders() {
        let mut book = Book::new();
        book.process(sell(1, 100, 5));
        book.process(buy(2, 100, 5)); // clears book
        assert!(book.asks_snapshot().is_empty());
        // Now submit new orders — book should behave as fresh.
        book.process(sell(3, 200, 3));
        let fills = book.process(buy(4, 200, 3));
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].qty, 3);
        assert!(book.asks_snapshot().is_empty());
    }
}
