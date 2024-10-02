use std::collections::{BTreeMap,VecDeque };
use std::cmp::{min, Ordering};
use std::rc::Rc;
use crate::comparable_price::ComparablePrice;
use crate::types::{Price, OrderConditions , PRICE_UNCHANGED , MARKET_ORDER_PRICE};
use crate::order::Order;
use crate::callback::Callback;
use crate::order_tracker::OrderTracker;
use crate::order_listener::OrderListener;
use crate::trade_listener::TradeListener;
use crate::order_book_listener::OrderBookListener;

//use crate::order_tracker::OrderTracker;
//use crate::listener::{OrderListener, TradeListener, OrderBookListener};

pub struct OrderBook<O: Order + Clone> {
    symbol: String,
    bids: BTreeMap<ComparablePrice, OrderTracker<O>>,
    asks: BTreeMap<ComparablePrice, OrderTracker<O>>,
    stop_bids: BTreeMap<ComparablePrice, OrderTracker<O>>,
    stop_asks: BTreeMap<ComparablePrice, OrderTracker<O>>,
    pending_orders: Vec<OrderTracker<O>>,
    callbacks: VecDeque<Callback<O>>,
    order_listener: Option<Box<dyn OrderListener<O>>>,
    trade_listener: Option<Box<dyn TradeListener<O>>>,
    order_book_listener: Option<Box<dyn OrderBookListener<O>>>,
    market_price: Price,
}

impl<O: Order + Clone> OrderBook<O> {
    pub fn new(symbol: String) -> Self {
        Self {
            symbol,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            stop_bids: BTreeMap::new(),
            stop_asks: BTreeMap::new(),
            pending_orders: Vec::new(),
            callbacks: VecDeque::new(),
            order_listener: None,
            trade_listener: None,
            order_book_listener: None,
            market_price: 0,
        }
    }

    pub fn set_symbol(&mut self, symbol: String) {
        self.symbol = symbol;
    }

    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    pub fn set_order_listener(&mut self, listener: Box<dyn OrderListener<O>>) {
        self.order_listener = Some(listener);
    }

    pub fn set_trade_listener(&mut self, listener: Box<dyn TradeListener<O>>) {
        self.trade_listener = Some(listener);
    }

    pub fn set_order_book_listener(&mut self, listener: Box<dyn OrderBookListener<O>>) {
        self.order_book_listener = Some(listener);
    }

    /// Adds an order to the order book
    pub fn add(&mut self, order: Rc<O>, conditions: OrderConditions) -> bool {
        let mut matched = false;

        if order.order_qty() == 0 {
            self.callbacks.push_back(Callback::reject(order, "size must be positive"));
        } else {
            let mut inbound = OrderTracker::new(order.clone(), conditions);
            
            if inbound.ptr().stop_price() != 0 && self.add_stop_order(inbound) {
                // The order has been added to stops
                self.callbacks.push_back(Callback::accept_stop(order));
            } else {
                let accept_cb_index = self.callbacks.len();
                self.callbacks.push_back(Callback::accept(order.clone()));
                matched = self.submit_order(&mut inbound);
                // Note the filled qty in the accept callback
                if let Some(callback) = self.callbacks.get_mut(accept_cb_index) {
                    callback.quantity = inbound.filled_qty();
                }

                // Cancel any unfilled IOC order
                if inbound.immediate_or_cancel() && !inbound.filled() {
                    // NOTE - this may need the actual open qty
                    self.callbacks.push_back(Callback::cancel(order.clone(), 0));
                }
            }

            // If adding this order triggered any stops
            // handle those stops now
            while !self.pending_orders.is_empty() {
                self.submit_pending_orders();
            }

            self.callbacks.push_back(Callback::book_update(Some(self)));
        }

        self.callback_now();
        matched
    }
    
    fn add_stop_order(&mut self, tracker: OrderTracker<O>) -> bool {
        let is_buy = tracker.ptr().is_buy();
        let key = ComparablePrice::new(is_buy, tracker.ptr().stop_price());
        // if the market price is a better deal than the stop price, it's not time to trigger
        let is_stopped = key < self.market_price;
        
        if is_stopped {
            if is_buy {
                self.stop_bids.insert(key, tracker);
            } else {
                self.stop_asks.insert(key, tracker);
            }
        }
        
        is_stopped
    }

    fn check_stop_orders(&mut self, side: bool, price: Price) {
        let until = ComparablePrice::new(side, price);
        let stops = if side { &mut self.stop_bids } else { &mut self.stop_asks };

        let mut triggered_stops = Vec::new();
        let mut keys_to_remove = Vec::new();

        for (key, _) in stops.range(..until) {
            if &until > key {
                break;
            }
            keys_to_remove.push(*key);
        }

        for key in keys_to_remove {
            if let Some(tracker) = stops.remove(&key) {
                triggered_stops.push(tracker);
            }
        }

        self.pending_orders.extend(triggered_stops);
    }

    fn submit_order(&mut self, inbound: &mut OrderTracker<O>) -> bool {
        let order_price = inbound.ptr().price();
        self.add_order(inbound, order_price)
    }

    // Note: You'll need to implement the add_order method as well
    fn add_order(&mut self, inbound: &mut OrderTracker<O>, order_price: Price) -> bool {
        // Implementation of add_order goes here
        todo!("Implement add_order method")
    }

    fn submit_pending_orders(&mut self) {
        // Take ownership of pending_orders, leaving an empty Vec in its place
        let pending = std::mem::take(&mut self.pending_orders);

        for mut tracker in pending {
            self.submit_order(&mut tracker);
            self.callbacks.push_back(Callback::trigger_stop(tracker.ptr().clone()));
        }

        // No need to swap back, as pending is dropped here and self.pending_orders is already empty
    }

    fn callback_now(&mut self) {
        // Protect against recursive calls
        if self.handling_callbacks {
            return;
        }

        self.handling_callbacks = true;

        // Process all accumulated callbacks
        while !self.callbacks.is_empty() {
            let mut working_callbacks = VecDeque::new();
            std::mem::swap(&mut self.callbacks, &mut working_callbacks);

            for callback in working_callbacks {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    self.perform_callback(callback);
                })) {
                    Ok(_) => {},
                    Err(e) => {
                        if let Some(logger) = &self.logger {
                            if let Some(error_message) = e.downcast_ref::<String>() {
                                logger.log_exception(&format!("Caught exception during callback: {}", error_message));
                            } else {
                                logger.log_message("Caught unknown exception during callback");
                            }
                        } else {
                            if let Some(error_message) = e.downcast_ref::<String>() {
                                eprintln!("Caught exception during callback: {}", error_message);
                            } else {
                                eprintln!("Caught unknown exception during callback");
                            }
                        }
                    }
                }
            }
        }

        self.handling_callbacks = false;
    }
    /// Cancel an order in the book
    pub fn cancel(&mut self, order: &O) {
        let mut found = false;
        let mut found_stop = false;
        let mut open_qty = 0;

        if order.is_buy() {
            if let Some(bid) = self.find_on_market(order) {
                open_qty = bid.open_qty();
                self.bids.remove(&ComparablePrice::new(true, order.price()));
                found = true;
            } else if order.stop_price() != 0 {
                if let Some(_) = self.find_in_stop_orders(order) {
                    self.stop_bids.remove(&ComparablePrice::new(true, order.stop_price()));
                    found_stop = true;
                }
            }
        } else {
            if let Some(ask) = self.find_on_market(order) {
                open_qty = ask.open_qty();
                self.asks.remove(&ComparablePrice::new(false, order.price()));
                found = true;
            } else if order.stop_price() != 0 {
                if let Some(_) = self.find_in_stop_orders(order) {
                    self.stop_asks.remove(&ComparablePrice::new(false, order.stop_price()));
                    found_stop = true;
                }
            }
        }

        if found {
            self.callbacks.push_back(Callback::cancel(order.clone(), open_qty));
            self.callbacks.push_back(Callback::book_update());
        } else if found_stop {
            self.callbacks.push_back(Callback::cancel_stop(order.clone()));
            self.callbacks.push_back(Callback::book_update());
        } else {
            self.callbacks.push_back(Callback::cancel_reject(order.clone(), "not found"));
        }

        self.callback_now();
    }

    pub fn replace(&mut self, order: &O, size_delta: i64, new_price: Price) -> bool {
        let mut matched = false;
        let price_change = new_price != 0 && new_price != order.price();

        let price = if new_price == PRICE_UNCHANGED { order.price() } else { new_price };

        let market = if order.is_buy() { &mut self.bids } else { &mut self.asks };
        
        if let Some(pos) = self.find_on_market(order) {
            let tracker = pos.1;
            let mut size_delta = size_delta;

            // If there is not enough open quantity for the size reduction
            if size_delta < 0 && (tracker.open_qty() as i64) < -size_delta {
                // get rid of as much as we can
                size_delta = -(tracker.open_qty() as i64);
                if size_delta == 0 {
                    // if there is nothing to get rid of
                    // Reject the replace
                    self.callbacks.push_back(Callback::replace_reject(order.clone(), 
                        "order is already filled"));
                    return false;
                }
            }

            // Accept the replace
            self.callbacks.push_back(
                Callback::replace(order.clone(), tracker.open_qty(), size_delta, price));
            
            let new_open_qty = tracker.open_qty() as i64 + size_delta;
            tracker.change_qty(size_delta);  // Update our copy
            
            // If the size change will close the order
            if new_open_qty == 0 {
                // Cancel with NO open qty (should be zero after replace)
                self.callbacks.push_back(Callback::cancel(order.clone(), 0));
                market.remove(&ComparablePrice::new(order.is_buy(), order.price()));
            } else {
                // Else rematch the new order - there could be a price change
                // or size change - that could cause all or none match
                let order_tracker = tracker.clone();
                market.remove(&ComparablePrice::new(order.is_buy(), order.price()));
                matched = self.add_order(&order_tracker, price);
            }

            // If replace any order this order triggered any trades
            // which triggered any stops
            // handle those stops now
            while !self.pending_orders.is_empty() {
                self.submit_pending_orders();
            }

            self.callbacks.push_back(Callback::book_update());
        } else {
            // not found
            self.callbacks.push_back(Callback::replace_reject(order.clone(), "not found"));
        }

        self.callback_now();
        matched
    }

    pub fn set_market_price(&mut self, price: Price) {
        let old_market_price = self.market_price;
        self.market_price = price;
        
        if price > old_market_price || old_market_price == MARKET_ORDER_PRICE {
            // price has gone up: check stop bids
            let buy_side = true;
            self.check_stop_orders(buy_side, price, &mut self.stop_bids);
        } else if price < old_market_price || old_market_price == MARKET_ORDER_PRICE {
            // price has gone down: check stop asks
            let buy_side = false;
            self.check_stop_orders(buy_side, price, &mut self.stop_asks);
        }
    }

    /// Get current market price 
    /// /// The market price is normally the price at which the last trade happened.
    pub fn market_price(&self) -> Price {
        self.market_price
    }


}    