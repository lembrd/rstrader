
simple strategy logic in naive_mm:
1) get mid_price from obs snapshot using formula = (top_bid * top_ask_amount + top_ask * top_bid_amount) / (top_bid_amount + top_ask_amount) 
2) add config param max_position, default 0.015
3) add config param lot_size, default = 0.005
4) add config param spread = 1.5 (bps) and displace_th = 1.0 (bps)
4) implement trading logic: if thereis no order for side (bid/ask), we post  order with price (target_price) = round_to_tick( mid_price - (spread / 10000 * mid_price) * order_side) for now tick value is hardcoded = 0.1. if position for that size is less than max_position. i.e. position.amount < config.max_position * orider.size, otherwise no post or cancel.
5) we cancel order if new target price counted from (4) differs with current order price more than config.displace_th /10000 * mid_price or position limit reached.
6) during strategy startup we send cancel all orders for symbol request to be sure that theris no orphaned orders. Keep in mind order response/execution reports. Use cl_ord_id as described in @oms.md 

api for sending orders should be implemented in account_state; When strategy start_trading account it should store reference on account API interface. And this interface should have methods like send_request(); Also strategy should receive RequestResponse for rest responses. This is unified api. All exchnage specific code should be placed in exchnages modules.

fix trade account for binance - replace cancelAll method to use rest API instead of ws. WS api used only for post/cancel methods. Also make sure our trading account api works correctly and use keys from naive_mm.yaml correctly, if need fix config credentials Make sure naive_mm is working as expected