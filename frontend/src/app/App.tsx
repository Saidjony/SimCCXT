import { useEffect } from "react";
import { TradingTerminal } from "./TradingTerminal";
import { TraderDiary } from "../components/TraderDiary";
import { preloadSim } from "../websocket/connection";
import { CHART_TIMEFRAMES } from "../chart/MiniChart";
import { getState, useSim } from "../state/store";
import { traderFromSearch } from "../trade/diary";

/**
 * Background market-data loader.
 *
 * When the socket is open, ask Rust to warm the disk caches for the whole
 * watchlist x every timeframe at once, so clicking any coin or any timeframe
 * resolves from disk instead of waiting for a download. This is deliberately
 * a `preload` and not a burst of `subscribe`s: a subscribe switches the
 * session's live series and answers with a full snapshot, so a burst would
 * fight the chart the user is actually looking at.
 *
 * Re-runs (debounced) when the socket opens, the watchlist changes or the
 * replay date window moves — the warm-up must cover the current selection.
 * The selectors below are what makes the component re-render on those changes.
 */
export function useBackgroundPreload(): void {
  const status = useSim((s) => s.status);
  const symbols = useSim((s) => s.watchlist.map((coin) => coin.symbol).join("|"));
  const exchange = useSim((s) => s.exchange);
  const startMs = useSim((s) => s.replayWindow.start_ms);
  const endMs = useSim((s) => s.replayWindow.end_ms);

  useEffect(() => {
    if (status !== "open" || symbols.length === 0) return;
    // Debounced: adding coins in a row must not queue one warm-up per keystroke.
    const timer = window.setTimeout(() => {
      const state = getState();
      preloadSim(
        state.watchlist.map((coin) => coin.symbol),
        CHART_TIMEFRAMES,
        state.exchange,
      );
    }, 400);
    return () => window.clearTimeout(timer);
  }, [status, symbols, exchange, startMs, endMs]);
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
