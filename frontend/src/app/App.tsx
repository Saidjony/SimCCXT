import { useEffect } from "react";
import { TradingTerminal } from "./TradingTerminal";
import { TraderDiary } from "../components/TraderDiary";
import { lookupMarketsSim, warmMarketData } from "../websocket/connection";
import { getState, useSim } from "../state/store";
import { traderFromSearch } from "../trade/diary";

/**
 * Background market-data loader.
 *
 * Downloads the 5m history of every watchlist coin into THIS browser's cache
 * (OPFS + IndexedDB manifest), one coin at a time, so switching coins later
 * resolves locally instead of waiting for the venue. It is deliberately not a
 * burst of `subscribe`s: a subscribe rebuilds the replay engine and its clock,
 * so a burst would fight the chart the user is actually looking at.
 *
 * Re-runs (debounced) when the watchlist or the replay date window changes.
 * The venue is NOT a dependency: switching venue must not re-download the
 * previous venue's history for coins the user is not looking at.
 */
export function useBackgroundPreload(): void {
  const status = useSim((s) => s.status);
  const symbols = useSim((s) => s.watchlist.map((coin) => coin.symbol).join("|"));
  const exchange = useSim((s) => s.exchange);
  const startMs = useSim((s) => s.replayWindow.start_ms);
  const endMs = useSim((s) => s.replayWindow.end_ms);

  useEffect(() => {
    if (status !== "open" || symbols.length === 0) return;
    // Debounced: adding coins in a row must not queue one download per keystroke.
    const timer = window.setTimeout(() => {
      warmMarketData(getState().watchlist.map((coin) => coin.symbol));
    }, 400);
    return () => window.clearTimeout(timer);
  }, [status, symbols, startMs, endMs]);

  // Fill the watchlist price ONCE per coin and venue. A periodic refresh made
  // every unselected coin's price move while one chart was replaying, which
  // looked like the replay was running for the whole watchlist. From then on
  // only the selected coin updates: its row shows the replay price.
  useEffect(() => {
    if (status !== "open" || symbols.length === 0) return;
    const state = getState();
    const missing = state.watchlist
      .map((coin) => coin.symbol)
      .filter((symbol) => {
        const quote = state.marketQuotes[symbol];
        return !quote || quote.exchange !== state.exchange;
      });
    if (missing.length > 0) lookupMarketsSim(missing, state.exchange);
  }, [status, symbols, exchange]);
}

/**
 * App = router on ONE query param: `?trader=<nick>` opens the read-only diary
 * of an online trader (pure HTTP, no WebSocket — the session belongs to the
 * original tab), anything else is the terminal. The preload hook lives in
 * `Terminal` so hooks are never called conditionally here.
 */
export function App(): JSX.Element {
  const trader = traderFromSearch(window.location.search);
  if (trader) return <TraderDiary nick={trader} />;
  return <Terminal />;
}

function Terminal(): JSX.Element {
  useBackgroundPreload();
  return <TradingTerminal />;
}
