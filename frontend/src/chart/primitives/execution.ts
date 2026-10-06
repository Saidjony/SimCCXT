/**
 * Execution context drawn on the chart: entry→exit legs of closed trades,
 * the open position's entry line (with its anchor bar) and resting stop
 * triggers. These used to be an SVG/HTML layer over the canvas; as a series
 * primitive they pan, zoom and screenshot with the chart.
 */
import { LineStyle } from "@tradingview/lwc-toolkit/line-style";
import type { CanvasRenderingTarget2D } from "fancy-canvas";
import { pairTradeFills } from "../overlays";
import { formatTs, type FillLine, type OrderLine, type PositionLine } from "../../types/protocol";
import { TransientPrimitive, type PrimitiveHost } from "./base";
import { C, drawChip, fillCircle, strokeSegment, type Conv, type Scope } from "./core";

export interface ExecutionData {
  fills: FillLine[];
  positions: PositionLine[];
  orders: OrderLine[];
  symbol: string;
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

function drawLinks(scope: Scope, conv: Conv, data: ExecutionData): void {
  for (const link of pairTradeFills(data.fills, data.symbol)) {
    const x1 = conv.x(link.entryTimestamp);
    const y1 = conv.y(link.entryPrice);
    const x2 = conv.x(link.exitTimestamp);
    const y2 = conv.y(link.exitPrice);
    if (x1 === null || y1 === null || x2 === null || y2 === null) continue;
    const color = link.direction === "long" ? C.green : C.red;
    strokeSegment(scope, x1, y1, x2, y2, color, 2, LineStyle.Dashed);
    fillCircle(scope, x1, y1, 3, color);
    fillCircle(scope, x2, y2, 3, color);
  }
}

function drawPositions(scope: Scope, conv: Conv, data: ExecutionData): void {
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
    const x = conv.x(position.entryTimeMs as number);
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
    target.useBitmapCoordinateSpace((scope) => {
      drawLinks(scope, conv, data);
      drawPositions(scope, conv, data);
      drawOrders(scope, conv, data);
    });
  }
}
