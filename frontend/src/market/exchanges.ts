// Direct exchange REST access from the browser.
//
// The terminal downloads its own market data: no candle or open-interest
// request ever leaves the user's browser for our server, and our server keeps
// no coin data at all. All three venues send permissive CORS headers on these
// public endpoints (Binance `Access-Control-Allow-Origin: *`, Bybit and OKX echo
// the request Origin), so a plain `fetch` is enough — no proxy, no CORS
// workaround, no keys.
//
// Only 5m klines are requested (BASE_TIMEFRAME); 1h and 1d are aggregated
// locally (./aggregate.ts).

import type { CandleRow, ExchangeId } from "../types/protocol.ts";

/** Most rows one venue returns per request. */
export const MAX_BARS_PER_REQUEST = 1000;
/** Weight budget: Binance allows 6000/min per IP and a kline page costs 2
 *  (limit ≤ 1000). Staying under a third of it leaves room for the OI pane and
 *  for a few tabs sharing one NAT address. */
const REQUEST_BUDGET_PER_MINUTE = 2000;

export interface SeriesRequest {
  exchange: ExchangeId;
  /** `BTC/USDT` — the terminal's unified form. */
  symbol: string;
  startMs: number;
  endMs: number;
  signal?: AbortSignal;
}

function venueSymbol(symbol: string): { base: string; quote: string; pair: string } {
  const [base, quote] = symbol.split("/");
  return { base, quote, pair: `${base}${quote}` };
}

/** One page of klines for a venue: normalized, ascending, deduped. */
function klineUrl(exchange: ExchangeId, symbol: string, startMs: number, endMs: number, limit: number): string {
  const { pair } = venueSymbol(symbol);
  switch (exchange) {
    case "binance":
      return "https://api.binance.com/api/v3/klines"
        + `?symbol=${pair}&interval=5m&limit=${limit}`
        + `&startTime=${startMs}&endTime=${endMs}`;
    case "bybit":
      return "https://api.bybit.com/v5/market/kline"
        + `?category=spot&symbol=${pair}&interval=5&limit=${limit}`
        + `&start=${startMs}&end=${endMs}`;
    case "okx": {
      const instId = pair.replace(/[A-Z]+$/, (quote) => `-${quote}`);
      return "https://www.okx.com/api/v5/market/history-candles"
        + `?instId=${instId}&bar=5m&limit=${Math.min(limit, 100)}&after=${startMs}`;
    }
  }
}

interface Page {
  rows: CandleRow[];
  /** Latest timestamp seen in the raw page (drives the pagination cursor). */
  latestMs: number;
}

function number(value: unknown): number | null {
  const n = typeof value === "number" ? value : Number(value);
  return Number.isFinite(n) ? n : null;
}

function row(ts: unknown, o: unknown, h: unknown, l: unknown, c: unknown, v: unknown): CandleRow | null {
  const values = [number(ts), number(o), number(h), number(l), number(c), number(v)];
  if (values.some((value) => value === null)) return null;
  return values as CandleRow;
}

async function fetchJson(url: string, signal?: AbortSignal): Promise<unknown> {
  const response = await fetch(url, { signal, headers: { Accept: "application/json" } });
  if (!response.ok) {
    throw new Error(`HTTP ${response.status} from ${new URL(url).hostname}`);
  }
  const body: unknown = await response.json();
  // Venue-level errors arrive with HTTP 200.
  const err = body as { code?: unknown; msg?: unknown; retCode?: unknown; retMsg?: unknown };
  if (typeof err.code === "string" && err.code !== "0") {
    throw new Error(String(err.msg ?? `code ${err.code}`));
  }
  if (typeof err.retCode === "number" && err.retCode !== 0) {
    throw new Error(String(err.retMsg ?? `retCode ${err.retCode}`));
  }
  return body;
}

function parseBinancePage(body: unknown): Page {
  if (!Array.isArray(body)) throw new Error("unexpected kline payload");
  const rows: CandleRow[] = [];
  let latestMs = 0;
  for (const entry of body) {
    if (!Array.isArray(entry)) continue;
    const candle = row(entry[0], entry[1], entry[2], entry[3], entry[4], entry[5]);
    if (!candle) continue;
    rows.push(candle);
    latestMs = Math.max(latestMs, candle[0]);
  }
  return { rows, latestMs };
}

function parseBybitPage(body: unknown): Page {
  const list = (body as { result?: { list?: unknown } }).result?.list;
  if (!Array.isArray(list)) throw new Error("unexpected kline payload");
  const rows: CandleRow[] = [];
  let latestMs = 0;
  for (const entry of list) {
    if (!Array.isArray(entry)) continue;
    const candle = row(entry[0], entry[1], entry[2], entry[3], entry[4], entry[5]);
    if (!candle) continue;
    rows.push(candle);
    latestMs = Math.max(latestMs, candle[0]);
  }
  return { rows, latestMs };
}

function parseOkxPage(body: unknown): Page {
  const data = (body as { data?: unknown }).data;
  if (!Array.isArray(data)) throw new Error("unexpected kline payload");
  const rows: CandleRow[] = [];
  let latestMs = 0;
  for (const entry of data) {
    if (!Array.isArray(entry)) continue;
    const candle = row(entry[0], entry[1], entry[2], entry[3], entry[4], entry[5]);
    if (!candle) continue;
    rows.push(candle);
    latestMs = Math.max(latestMs, candle[0]);
  }
  return { rows, latestMs };
}

function parsePage(exchange: ExchangeId, body: unknown): Page {
  switch (exchange) {
    case "binance":
      return parseBinancePage(body);
    case "bybit":
      return parseBybitPage(body);
    case "okx":
      return parseOkxPage(body);
  }
}

/** Thrown when a venue answers 429/418: the caller must back off, not retry. */
export class RateLimitedError extends Error {}

/**
 * Download 5m klines for [startMs, endMs] page by page.
 *
 * Public venues cap a page at 1000 bars and a rate limit per IP; the loop is
 * therefore sequential (never a burst), advances by the newest timestamp it
 * received, and stops on an empty or non-advancing page so a venue quirk can
 * never spin forever.
 */
export async function downloadBars(
  request: SeriesRequest,
  onProgress?: (done: number, total: number) => void,
): Promise<CandleRow[]> {
  const { exchange, symbol, startMs, endMs, signal } = request;
  if (!(startMs < endMs)) return [];
  const step = 300_000;
  const expected = Math.ceil((endMs - startMs) / step);
  const byTime = new Map<number, CandleRow>();
  let cursor = startMs;
  let requests = 0;

  while (cursor <= endMs) {
    if (signal?.aborted) throw new DOMException("aborted", "AbortError");
    if (requests > 0 && requests > 200) break; // hard stop: >200k bars one call
    const limit = MAX_BARS_PER_REQUEST;
    const url = klineUrl(exchange, symbol, cursor, endMs, limit);
    let page: Page;
    try {
      const body = await fetchJson(url, signal);
      page = parsePage(exchange, body);
    } catch (error) {
      if (error instanceof DOMException && error.name === "AbortError") throw error;
      const message = error instanceof Error ? error.message : String(error);
      if (message.includes("429") || message.includes("418")) {
        throw new RateLimitedError(`${exchange}: rate limited (${message})`);
      }
      throw error;
    }
    requests += 1;
    if (page.rows.length === 0) break;
    for (const candle of page.rows) {
      if (candle[0] >= startMs && candle[0] <= endMs) byTime.set(candle[0], candle);
    }
    onProgress?.(byTime.size, expected);
    if (page.latestMs < cursor) break; // venue did not advance
    cursor = page.latestMs + step;
    if (page.rows.length < limit && page.rows.length < 100) {
      // A short page means the venue has nothing more in this range.
      if (exchange !== "okx") break;
    }
    if (requests * 2 >= REQUEST_BUDGET_PER_MINUTE) {
      // Weight budget spent: let the venue's window roll over before the next
      // page instead of hammering it into a 429.
      await new Promise((resolve) => setTimeout(resolve, 60_000));
      requests = 0;
    }
  }

  const rows = [...byTime.values()].sort((a, b) => a[0] - b[0]);
  if (rows.length === 0) {
    throw new Error(`${exchange} has no ${symbol} candles between the selected dates`);
  }
  return rows;
}

/** Current public price per symbol (watchlist quotes). */
export async function downloadTicker(exchange: ExchangeId, symbol: string): Promise<number | null> {
  const { pair } = venueSymbol(symbol);
  const url = exchange === "binance"
    ? `https://api.binance.com/api/v3/ticker/price?symbol=${pair}`
    : exchange === "bybit"
      ? `https://api.bybit.com/v5/market/tickers?category=spot&symbol=${pair}`
      : `https://www.okx.com/api/v5/market/ticker?instId=${pair.replace(/([A-Z]+)$/, "-$1")}`;
  const body = await fetchJson(url);
  if (exchange === "binance") {
    return number((body as { price?: unknown }).price);
  }
  if (exchange === "bybit") {
    const list = (body as { result?: { list?: Array<{ lastPrice?: unknown }> } }).result?.list;
    return number(list?.[0]?.lastPrice);
  }
  const data = (body as { data?: Array<{ last?: unknown }> }).data;
  return number(data?.[0]?.last);
}

// ---------------------------------------------------------------------------
// Open interest
// ---------------------------------------------------------------------------

export interface OiSeries {
  samples: Array<[number, number]>;
  /** Grid the venue served ("5m", "15m", "1h", ...), for the pane legend. */
  period: string;
}

/** OI only exists on derivative books; these URLs are the swap endpoints. */
export function oiUrl(exchange: ExchangeId, symbol: string, period: string, limit: number): string {
  const { pair } = venueSymbol(symbol);
  switch (exchange) {
    case "binance":
      return `https://fapi.binance.com/futures/data/openInterestHist?symbol=${pair}&period=${period}&limit=${limit}`;
    case "bybit":
      return `https://api.bybit.com/v5/market/open-interest?category=linear&symbol=${pair}`
        + `&intervalTime=${period}&limit=${Math.min(limit, 200)}`;
    case "okx":
      return `https://www.okx.com/api/v5/rubik/stat/contracts/open-interest-volume`
        + `?ccy=${venueSymbol(symbol).base}&period=${period === "1d" ? "1D" : period}`;
  }
}

/**
 * Download open-interest history on the grid that matches `timeframe`.
 *
 * A venue that does not expose OI for the symbol yields an empty series (the
 * pane then says "no OI data"); it is never invented.
 */
export async function downloadOi(
  exchange: ExchangeId,
  symbol: string,
  period: "5m" | "1h" | "1d",
  signal?: AbortSignal,
): Promise<OiSeries> {
  const limit = period === "5m" ? 500 : 100;
  try {
    const body = await fetchJson(oiUrl(exchange, symbol, period, limit), signal);
    return { samples: parseOi(exchange, body), period };
  } catch (error) {
    if (error instanceof DOMException && error.name === "AbortError") throw error;
    return { samples: [], period };
  }
}

function parseOi(exchange: ExchangeId, body: unknown): Array<[number, number]> {
  const samples: Array<[number, number]> = [];
  const push = (ts: unknown, value: unknown): void => {
    const time = number(ts);
    const interest = number(value);
    if (time !== null && interest !== null && interest >= 0) samples.push([time, interest]);
  };
  if (exchange === "binance") {
    for (const entry of Array.isArray(body) ? body : []) {
      const line = entry as { timestamp?: unknown; sumOpenInterestValue?: unknown; sumOpenInterest?: unknown };
      push(line.timestamp, line.sumOpenInterestValue ?? line.sumOpenInterest);
    }
  } else if (exchange === "bybit") {
    const list = (body as { result?: { list?: unknown } }).result?.list;
    for (const entry of Array.isArray(list) ? list : []) {
      const line = entry as { timestamp?: unknown; openInterest?: unknown };
      push(line.timestamp, line.openInterest);
    }
  } else {
    const data = (body as { data?: unknown }).data;
    for (const entry of Array.isArray(data) ? data : []) {
      if (!Array.isArray(entry)) continue;
      push(entry[0], entry[2]); // [ts, oi, oiValue, volume]
    }
  }
  return samples.sort((a, b) => a[0] - b[0]);
}
