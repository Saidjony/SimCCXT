/**
 * Shared plumbing for the chart drawing primitives.
 *
 * Every drawing is rendered by lightweight-charts itself, inside its own
 * canvas, as a series primitive (ISeriesPrimitive). There is no HTML/SVG
 * overlay layer on top of the chart: HTML is reserved for UI (toolbar,
 * dialogs, hints). Coordinates always come from the chart's own conversion
 * (time -> coordinate, price -> coordinate), so a drawing can never drift
 * away from the bars it belongs to.
 */
import type { BitmapCoordinatesRenderingScope, CanvasRenderingTarget2D } from "fancy-canvas";
import { positionsBox, positionsLine } from "@tradingview/lwc-toolkit/dimensions/positions";
import { drawRoundRectWithBorder } from "@tradingview/lwc-toolkit/canvas/round-rect";
import { getDashPattern, LineStyle } from "@tradingview/lwc-toolkit/line-style";
import type {
  Coordinate,
  IChartApi,
  IPrimitivePaneRenderer,
  IPrimitivePaneView,
  ISeriesApi,
  ITimeScaleApi,
  PrimitivePaneViewZOrder,
  SeriesOptionsMap,
  Time,
  UTCTimestamp,
} from "lightweight-charts";
import { nearestCandleTime } from "../overlays";

/**
 * Magnet mode: when on, the price coordinate snaps to the nearest OHLC value
 * of the bar under the cursor (TradingView-style magnet). Time always snaps
 * to the bar. The Chart component syncs this from its state.
 */
let magnetEnabled = false;

export function setMagnetMode(v: boolean): void {
  magnetEnabled = v;
}

export function isMagnetEnabled(): boolean {
  return magnetEnabled;
}

/** Palette mirrored from styles.css so canvas and UI speak one colour language. */
export const C = {
  bg: "#0b0e13",
  text: "#c9d1d9",
  accent: "#4a9ec4",
  green: "#3d9a50",
  red: "#d46666",
  yellow: "#e0af68",
  line: "#222c3b",
  chip: "rgba(11, 14, 19, 0.75)",
} as const;

export const FONT_FAMILY =
  'ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif';

export interface Pt {
  /** Bar time, ms. */
  t: number;
  /** Price. */
  p: number;
}

export type Scope = BitmapCoordinatesRenderingScope;

/**
 * Coordinate bridge between chart data and the pane's canvas: every drawing
 * converts through the chart, never through container pixel math of its own.
 */
export interface Conv {
  /** Pane x of a bar time (snapped to this pane's own candle grid), or null. */
  x(ms: number): Coordinate | null;
  /** Pane y of a price, or null when the series cannot place it. */
  y(price: number): Coordinate | null;
  /** Pane width in CSS pixels (excludes the price axis). */
  width(): number;
  /** Pane height in CSS pixels. */
  height(): number;
}

/** Hex colour with an alpha channel appended (canvas labels mirror CSS alphas). */
export function alpha(color: string, value: number): string {
  if (/^#[0-9a-fA-F]{6}$/.test(color)) {
    const a = Math.round(Math.max(0, Math.min(1, value)) * 255);
    return color + a.toString(16).padStart(2, "0");
  }
  return color;
}

/** A slightly brighter variant of a hex colour (the "selected" filter in CSS). */
export function brighten(color: string): string {
  if (!/^#[0-9a-fA-F]{6}$/.test(color)) return color;
  const step = (v: number): string =>
    Math.min(255, Math.round(v * 1.4)).toString(16).padStart(2, "0");
  const n = parseInt(color.slice(1), 16);
  return `#${step((n >> 16) & 255)}${step((n >> 8) & 255)}${step(n & 255)}`;
}

/** UTC HH:MM of a bar time — matches every other time label in the terminal. */
export function hhmm(ms: number): string {
  const d = new Date(ms);
  return `${String(d.getUTCHours()).padStart(2, "0")}:${String(d.getUTCMinutes()).padStart(2, "0")}`;
}

/** Epoch seconds as the library's bar time type. */
export function toTime(ms: number): UTCTimestamp {
  return Math.floor(ms / 1000) as UTCTimestamp;
}

/**
 * Build the coordinate bridge for one series. Bar times are snapped to this
 * pane's own candle grid first, so a drawing placed on a 1m bar still lands on
 * the right x inside a 15m panel (the mirror charts depend on this).
 */
export function makeConv(
  chart: IChartApi,
  series: ISeriesApi<keyof SeriesOptionsMap>,
  times: () => number[],
): Conv {
  return {
    x(ms) {
      const rows = times();
      if (rows.length === 0) return null;
      const snapped = nearestCandleTime(rows, ms);
      if (snapped === null) return null;
      const coord = chart.timeScale().timeToCoordinate(toTime(snapped));
      return typeof coord === "number" ? (coord as Coordinate) : null;
    },
    y(price) {
      const coord = series.priceToCoordinate(price);
      return typeof coord === "number" ? coord : null;
    },
    width: () => chart.timeScale().width(),
    height: () => series.getPane().getHeight(),
  };
}

/** Offscreen 2D context used to measure label text (created once, lazily). */
let measureCtx: CanvasRenderingContext2D | null = null;

/** Width of a text run in CSS pixels, at the chip's font. */
export function measureText(text: string, fontSize: number, weight = "400"): number {
  if (typeof document === "undefined") return text.length * fontSize * 0.6;
  if (!measureCtx) measureCtx = document.createElement("canvas").getContext("2d");
  if (!measureCtx) return text.length * fontSize * 0.6;
  measureCtx.font = `${weight} ${fontSize}px ${FONT_FAMILY}`;
  return measureCtx.measureText(text).width;
}

/** Hit-test tolerance in CSS pixels: generous enough for a mouse, tight enough to stay precise. */
export const HIT_TOLERANCE = 6;

// ---------------------------------------------------------------------------
// Strokes / fills (bitmap space, crisp on any device pixel ratio)
// ---------------------------------------------------------------------------

export function strokeSegment(
  scope: Scope,
  x1: number,
  y1: number,
  x2: number,
  y2: number,
  color: string,
  width = 1,
  style: LineStyle = LineStyle.Solid,
): void {
  const { context: ctx, horizontalPixelRatio: hr, verticalPixelRatio: vr } = scope;
  const bw = Math.max(1, Math.round(width * hr));
  ctx.save();
  ctx.strokeStyle = color;
  ctx.lineWidth = bw;
  ctx.lineCap = "butt";
  const pattern = getDashPattern(style, bw);
  if (pattern.length) ctx.setLineDash(pattern);
  if (x1 === x2 || y1 === y2) {
    // Round + centre through the toolkit's position helpers so a 1px line
    // lands on whole device pixels instead of smearing across two.
    ctx.beginPath();
    if (x1 === x2) {
      const v = positionsLine(x1, hr, bw, true);
      const ys = positionsBox(y1, y2, vr);
      ctx.moveTo(v.position + v.length / 2, ys.position);
      ctx.lineTo(v.position + v.length / 2, ys.position + ys.length);
    } else {
      const v = positionsLine(y1, vr, bw, true);
      const xs = positionsBox(x1, x2, hr);
      ctx.moveTo(xs.position, v.position + v.length / 2);
      ctx.lineTo(xs.position + xs.length, v.position + v.length / 2);
    }
    ctx.stroke();
  } else {
    ctx.beginPath();
    ctx.moveTo(x1 * hr, y1 * vr);
    ctx.lineTo(x2 * hr, y2 * vr);
    ctx.stroke();
  }
  ctx.restore();
}

export function fillRectBox(
  scope: Scope,
  left: number,
  top: number,
  width: number,
  height: number,
  color: string,
): void {
  const { context: ctx, horizontalPixelRatio: hr, verticalPixelRatio: vr } = scope;
  if (!(width > 0) || !(height > 0)) return;
  ctx.save();
  ctx.fillStyle = color;
  ctx.fillRect(
    Math.round(left * hr),
    Math.round(top * vr),
    Math.max(1, Math.round(width * hr)),
    Math.max(1, Math.round(height * vr)),
  );
  ctx.restore();
}

export function strokeRectBox(
  scope: Scope,
  left: number,
  top: number,
  width: number,
  height: number,
  color: string,
  lineWidth = 1,
  style: LineStyle = LineStyle.Solid,
): void {
  const { context: ctx, horizontalPixelRatio: hr, verticalPixelRatio: vr } = scope;
  if (!(width > 0) || !(height > 0)) return;
  const bw = Math.max(1, Math.round(lineWidth * hr));
  ctx.save();
  ctx.strokeStyle = color;
  ctx.lineWidth = bw;
  const pattern = getDashPattern(style, bw);
  if (pattern.length) ctx.setLineDash(pattern);
  const l = Math.round(left * hr) + bw / 2;
  const t = Math.round(top * vr) + bw / 2;
  const w = Math.max(1, Math.round(width * hr) - bw);
  const h = Math.max(1, Math.round(height * vr) - bw);
  ctx.strokeRect(l, t, w, h);
  ctx.restore();
}

export function fillCircle(
  scope: Scope,
  x: number,
  y: number,
  radius: number,
  color: string,
): void {
  const { context: ctx, horizontalPixelRatio: hr, verticalPixelRatio: vr } = scope;
  ctx.save();
  ctx.fillStyle = color;
  ctx.beginPath();
  ctx.arc(x * hr, y * vr, Math.max(1, radius * hr), 0, Math.PI * 2);
  ctx.fill();
  ctx.restore();
}

export function strokeCircle(
  scope: Scope,
  x: number,
  y: number,
  radius: number,
  color: string,
  lineWidth = 1,
): void {
  const { context: ctx, horizontalPixelRatio: hr, verticalPixelRatio: vr } = scope;
  ctx.save();
  ctx.strokeStyle = color;
  ctx.lineWidth = Math.max(1, Math.round(lineWidth * hr));
  ctx.beginPath();
  ctx.arc(x * hr, y * vr, Math.max(1, radius * hr), 0, Math.PI * 2);
  ctx.stroke();
  ctx.restore();
}

export interface ChipOptions {
  /** Font size in CSS pixels. */
  fontSize?: number;
  weight?: string;
  background: string;
  foreground?: string;
  border?: string;
  radius?: number;
  padX?: number;
  padY?: number;
  align?: "left" | "center" | "right";
  /** Vertical anchor of the chip relative to `y`. */
  vAlign?: "top" | "middle" | "bottom";
}

/**
 * Media-space box a chip would occupy at (x, y) — shared by the renderer and
 * the hit test so a label can never be clickable outside what you see.
 */
export function chipBounds(
  text: string,
  x: number,
  y: number,
  options: ChipOptions,
): { left: number; top: number; width: number; height: number } {
  const fontSize = options.fontSize ?? 11;
  const weight = options.weight ?? "400";
  const padX = options.padX ?? 6;
  const padY = options.padY ?? 2;
  const align = options.align ?? "left";
  const vAlign = options.vAlign ?? "top";
  const width = measureText(text, fontSize, weight) + padX * 2;
  const height = fontSize + padY * 2;
  const left = align === "center" ? x - width / 2 : align === "right" ? x - width : x;
  const top = vAlign === "middle" ? y - height / 2 : vAlign === "bottom" ? y - height : y;
  return { left, top, width, height };
}

/**
 * A rounded, filled text chip — the label style the terminal already uses
 * (note captions, position header, ruler readout). Drawn in the chart canvas.
 */
export function drawChip(
  scope: Scope,
  text: string,
  x: number,
  y: number,
  options: ChipOptions,
): void {
  const { context: ctx, horizontalPixelRatio: hr, verticalPixelRatio: vr } = scope;
  const fontSize = options.fontSize ?? 11;
  const weight = options.weight ?? "400";
  const padX = options.padX ?? 6;
  const align = options.align ?? "left";
  const radius = options.radius ?? 4;
  const box = chipBounds(text, x, y, options);

  ctx.save();
  const left = Math.round(box.left * hr);
  const top = Math.round(box.top * vr);
  const width = Math.max(1, Math.round(box.width * hr));
  const height = Math.max(1, Math.round(box.height * vr));
  drawRoundRectWithBorder(
    ctx,
    left,
    top,
    width,
    height,
    options.background,
    options.border ? Math.max(1, Math.round(hr)) : 0,
    [radius * hr, radius * hr, radius * hr, radius * hr],
    options.border,
  );
  ctx.font = `${weight} ${fontSize * hr}px ${FONT_FAMILY}`;
  ctx.fillStyle = options.foreground ?? C.text;
  ctx.textBaseline = "middle";
  ctx.textAlign = align === "center" ? "center" : align === "right" ? "right" : "left";
  const tx =
    align === "center"
      ? Math.round(left + width / 2)
      : align === "right"
        ? Math.round(left + width - padX * hr)
        : Math.round(left + padX * hr);
  ctx.fillText(text, tx, Math.round(top + height / 2));
  ctx.restore();
}

// ---------------------------------------------------------------------------
// Hit testing (CSS pixel space, same as priceToCoordinate/timeToCoordinate)
// ---------------------------------------------------------------------------

/** Distance from a point to a segment. */
export function distanceToSegment(
  px: number,
  py: number,
  x1: number,
  y1: number,
  x2: number,
  y2: number,
): number {
  const dx = x2 - x1;
  const dy = y2 - y1;
  const len2 = dx * dx + dy * dy;
  if (len2 === 0) return Math.hypot(px - x1, py - y1);
  let t = ((px - x1) * dx + (py - y1) * dy) / len2;
  t = Math.max(0, Math.min(1, t));
  return Math.hypot(px - (x1 + t * dx), py - (y1 + t * dy));
}

/** Distance from a point to a rectangle border/interior (0 when inside). */
export function distanceToRect(
  px: number,
  py: number,
  left: number,
  top: number,
  width: number,
  height: number,
): number {
  const dx = Math.max(left - px, 0, px - (left + width));
  const dy = Math.max(top - py, 0, py - (top + height));
  return Math.hypot(dx, dy);
}

// ---------------------------------------------------------------------------
// Pane views
// ---------------------------------------------------------------------------

/** A pane view whose renderer is produced fresh on every rebuild. */
export function makePaneView(
  renderer: () => IPrimitivePaneRenderer | null,
  zOrder: PrimitivePaneViewZOrder = "top",
): IPrimitivePaneView {
  return {
    renderer,
    zOrder: () => zOrder,
  };
}
