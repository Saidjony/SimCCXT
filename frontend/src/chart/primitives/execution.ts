/**
 * Execution context drawn on the chart: entry→exit legs of closed trades,
 * the open position's entry line (with its anchor bar) and resting stop
 * triggers. These used to be an SVG/HTML layer over the canvas; as a series
 * primitive they pan, zoom and screenshot with the chart.
 */
import { LineStyle } from "@tradingview/lwc-toolkit/line-style";
import type { CanvasRenderingTarget2D } from "fancy-canvas";
import {
  pairTradeFills,
  tradeBoxShape,
  tradeOutcome,
  tradeVisible,
  tradeZones,
  type TradeLink,
  type TradeOutcome,
} from "../overlays";
import { formatTs, findProtection, type FillLine, type OrderLine, type PositionLine } from "../../types/protocol";
import { TransientPrimitive, type PrimitiveHost } from "./base";
import {
  C,
  alpha,
  drawChip,
  fillCircle,
  fillRectBox,
  strokeRectBox,
  strokeSegment,
  type Conv,
  type Scope,
} from "./core";

export interface ExecutionData {
  fills: FillLine[];
  positions: PositionLine[];
  orders: OrderLine[];
  symbol: string;
  /** Planned stop/target per entry order id (`entry_protections` mirrored locally). */
  protections: Record<string, { stopPrice: number | null; takePrice: number | null }>;
}

const LABEL_BG = "rgba(11, 14, 19, 0.88)";

function labelChip(scope: Scope, text: string, right: number, top: number, color: string): void {
  drawChip(scope, text, right, top, {
    background: LABEL_BG,
    foreground: color,
    fontSize: 9,
    radius: 3,
    padX: 4,
    padY: 1,
    align: "right",
    vAlign: "top",
  });
}

function fmtPrice(price: number): string {
  return price.toLocaleString("en-US", { maximumFractionDigits: 2 });
}

/** `LONG +2.40R` when the stop is known, `SHORT -1.20%` otherwise. */
function outcomeLabel(link: TradeLink, outcome: TradeOutcome): string {
  const dir = link.direction === "long" ? "LONG" : "SHORT";
  if (outcome.r !== null) {
    return `${dir} ${outcome.r > 0 ? "+" : ""}${outcome.r.toFixed(2)}R`;
  }
  return `${dir} ${outcome.pct > 0 ? "+" : ""}${outcome.pct.toFixed(2)}%`;
}

/**
 * Closed trades as TradingView-style position boxes: a rectangle from entry
 * time to exit time, green on the reward side (planned target, or the realized
 * exit when it went our way) and red over the risk side (entry→stop), with
 * entry/exit/stop/target levels and prices labelled.
 *
 * Both legs are drawn independently of the outcome, so a stopped-out trade
 * still shows the profit zone it was aiming at instead of only a red block.
 */
function drawTradeBoxes(
  scope: Scope,
  conv: Conv,
  data: ExecutionData,
  times: ReadonlyArray<number>,
): void {
  for (const link of pairTradeFills(data.fills, data.symbol)) {
    // Fills are persisted across reloads; the replay is not. A trade whose
    // bars are still hidden must not leak onto the last revealed candle.
    if (!tradeVisible(link, times)) continue;
    const x1 = conv.x(link.entryTimestamp);
    const x2 = conv.x(link.exitTimestamp);
    const entryY = conv.y(link.entryPrice);
    const exitY = conv.y(link.exitPrice);
    if (x1 === null || x2 === null || entryY === null || exitY === null) continue;
    const protection = findProtection(
      data.protections,
      data.symbol,
      link.entryOrderId,
      link.entryTimestamp,
    );
    const stopPrice = protection?.stopPrice ?? null;
    const takePrice = protection?.takePrice ?? null;
    const stopY = stopPrice !== null ? conv.y(stopPrice) : null;
    const zones = tradeZones(link, stopPrice, takePrice);
    const rewardY = zones.rewardPrice !== null ? conv.y(zones.rewardPrice) : null;
    const adverseY = zones.adversePrice !== null ? conv.y(zones.adversePrice) : null;
    const box = tradeBoxShape(x1, entryY, x2, exitY, stopY, rewardY);
    const outcome = tradeOutcome(link, stopPrice);
    const color = outcome.profitable ? C.green : C.red;

    // Profit zone: entry → target (or the profitable exit).
    if (rewardY !== null) {
      fillRectBox(
        scope,
        box.left,
        Math.min(entryY, rewardY),
        box.width,
        Math.abs(rewardY - entryY),
        alpha(C.green, 0.16),
      );
    }
    // Risk zone: entry → stop, extended to a losing exit that ran past it.
    if (adverseY !== null) {
      fillRectBox(
        scope,
        box.left,
        Math.min(entryY, adverseY),
        box.width,
        Math.abs(adverseY - entryY),
        alpha(C.red, 0.14),
      );
    }

    strokeRectBox(scope, box.left, box.top, box.width, box.height, alpha(color, 0.9), 1);
    strokeSegment(
      scope,
      box.left,
      entryY,
      box.left + box.width,
      entryY,
      alpha(C.accent, 0.9),
      1,
      LineStyle.Dashed,
    );
    strokeSegment(scope, box.left, exitY, box.left + box.width, exitY, color, 1, LineStyle.Solid);
    if (stopY !== null) {
      strokeSegment(scope, box.left, stopY, box.left + box.width, stopY, C.red, 1, LineStyle.Dashed);
    }
    // The target line only when the exit did not already land on it.
    if (rewardY !== null && Math.abs(rewardY - exitY) > 0.5) {
      strokeSegment(
        scope,
        box.left,
        rewardY,
        box.left + box.width,
        rewardY,
        C.green,
        1,
        LineStyle.Dashed,
      );
    }
    fillCircle(scope, x1, entryY, 3, C.accent);
    fillCircle(scope, x2, exitY, 3, color);

    drawChip(scope, outcomeLabel(link, outcome), box.left + 4, box.top + 3, {
      background: LABEL_BG,
      foreground: color,
      fontSize: 10,
      weight: "700",
      radius: 4,
      padX: 5,
      padY: 1,
    });
    const stopText = stopPrice !== null ? ` · SL ${fmtPrice(stopPrice)}` : "";
    const takeText = takePrice !== null ? ` · TP ${fmtPrice(takePrice)}` : "";
    const detail = `in ${fmtPrice(link.entryPrice)} · out ${fmtPrice(link.exitPrice)}${stopText}${takeText}`;
    const detailTop = box.height >= 34 ? box.top + box.height - 14 : box.top + box.height + 2;
    drawChip(scope, detail, box.left + 4, Math.min(detailTop, conv.height() - 12), {
      background: LABEL_BG,
      foreground: C.text,
      fontSize: 9,
      radius: 3,
      padX: 4,
      padY: 1,
    });
  }
}

function drawPositions(
  scope: Scope,
  conv: Conv,
  data: ExecutionData,
  times: ReadonlyArray<number>,
): void {
  const right = conv.width() - 2;
  const bottom = conv.height() - 8;
  for (const position of data.positions) {
    if (position.symbol !== data.symbol) continue;
    const y = conv.y(position.entryPrice);
    if (y === null) continue;
    const color = position.side === "short" ? C.red : C.green;
    strokeSegment(scope, 0, y, conv.width(), y, C.yellow, 1, LineStyle.Dashed);
    const known = Boolean(position.entryTimeMs && position.entryTimeMs > 0);
    const when = known ? " · " + formatTs(position.entryTimeMs as number) : "";
    labelChip(
      scope,
      `${position.side.toUpperCase()} entry · ${position.entryPrice.toFixed(2)}${when}`,
      right,
      y - 12,
      C.yellow,
    );
    if (!known) continue;
    // An entry bar that the replay has not revealed yet gets no anchor line.
    const entryMs = position.entryTimeMs as number;
    if (!times.length || entryMs < times[0] || entryMs > times[times.length - 1]) continue;
    const x = conv.x(entryMs);
    if (x === null) continue;
    strokeSegment(scope, x, y, x, bottom, color, 1, LineStyle.Dashed);
    fillCircle(scope, x, y, 4, color);
  }
}

function drawOrders(scope: Scope, conv: Conv, data: ExecutionData): void {
  const right = conv.width() - 2;
  for (const order of data.orders) {
    if (order.symbol !== data.symbol || order.triggerPrice === null) continue;
    const y = conv.y(order.triggerPrice);
    if (y === null) continue;
    strokeSegment(scope, 0, y, conv.width(), y, C.red, 1, LineStyle.Dashed);
    labelChip(
      scope,
      `STOP ${order.side.toUpperCase()} · ${order.triggerPrice.toFixed(2)} · ${order.remaining}`,
      right,
      y - 12,
      C.red,
    );
  }
}

export class ExecutionPrimitive extends TransientPrimitive {
  private _data: ExecutionData | null = null;

  public setData(data: ExecutionData | null): void {
    this._data = data;
    this.requestUpdate();
  }

  protected drawPane(target: CanvasRenderingTarget2D, conv: Conv): void {
    const data = this._data;
    if (!data) return;
    const times = this._host.times();
    target.useBitmapCoordinateSpace((scope) => {
      drawTradeBoxes(scope, conv, data, times);
      drawPositions(scope, conv, data, times);
      drawOrders(scope, conv, data);
    });
  }
}
