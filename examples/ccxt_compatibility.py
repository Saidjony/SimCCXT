"""simccxt — CCXT-compatible virtual exchange example.

This script demonstrates that a strategy written against the CCXT interface
runs unchanged against simccxt.

Run (after building/installing the simccxt extension):
    python3 examples/ccxt_compatibility.py
"""

import simccxt


def run_strategy(exchange, name: str) -> None:
    """A minimal strategy that only uses the CCXT public interface."""
    symbol = "ETH/USDT"

    candles = exchange.fetch_ohlcv(symbol, "1m", limit=3)
    print(f"[{name}] candles: {len(candles)}")

    ticker = exchange.fetch_ticker(symbol)
    print(f"[{name}] last price: {ticker['last']}")

    book = exchange.fetch_order_book(symbol, limit=5)
    print(f"[{name}] order book bids: {len(book['bids'])}")

    order = exchange.create_order(symbol, "limit", "buy", 0.01, ticker["last"] * 0.9)
    print(f"[{name}] created order: {order['id']} status={order['status']}")

    open_orders = exchange.fetch_open_orders(symbol)
    print(f"[{name}] open orders: {len(open_orders)}")

    if order["status"] == "open":
        exchange.cancel_order(order["id"], symbol)

    balance = exchange.fetch_balance()
    print(f"[{name}] USDT total: {balance['total'].get('USDT', 0.0)}")


def main() -> None:
    exchange = simccxt.binance()  # <-- interchangeable with ccxt.binance()
    exchange.add_market("ETH/USDT", "spot")
    exchange.deposit("USDT", 100_000.0)
    exchange.deposit("ETH", 10.0)

    exchange.load_candles(
        "ETH/USDT",
        "1m",
        [
            [1704067200000, 2200.0, 2250.0, 2150.0, 2220.0, 10.0],
            [1704067260000, 2220.0, 2280.0, 2190.0, 2260.0, 12.0],
            [1704067320000, 2260.0, 2300.0, 2210.0, 2275.0, 8.0],
        ],
    )

    run_strategy(exchange, "simccxt")


if __name__ == "__main__":
    main()
