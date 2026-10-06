/**
 * Position plan (three clicks: entry, stop, target). Bands, entry line,
 * outer border and the R/size header are all painted inside the chart canvas.
 */
import { AxisLabelView, type AxisLabelSource } from "@tradingview/lwc-toolkit/axis-label-view";
import { LineStyle } from "@tradingview/lwc-toolkit/line-style";
import type { CanvasRenderingTarget2D } from "fancy-canvas";
import type { Coordinate, ISeriesPrimitiveAxisView } from "lightweight-charts";
import type { Drawing } from "../../state/store";
import { positionBands, positionDirection, riskReward, toBox } from "../overlays";
import { DrawingPrimitive, type HitResult, type PrimitiveHost } from "./base";
import {
  C,
  alpha,
  brighten,
  drawChip,
  distanceToRect,
  fillRectBox,
  strokeRectBox,
  strokeSegment,
  type Conv,
  type Scope,
} from "./core";

export type Position = Extract<Drawing, { kind: "position" }>;

/** Position shape without its store id: the ghost preview reuses it. */
export interface PositionShape {
  t1: number;
  t2: number;
  entry: number;
  sl: number;
  /** Absent while the third click is still pending. */
  tp: number | null;
}

export interface PositionGeometry {
  left: number;
  top: number;
  width: number;
  height: number;
  topPrice: number;
  bottomPrice: number;
  bands: Array<{ from: number; to: number; cls: "tp" | "sl" | "entry" }>;
}

export function positionGeometry(shape: PositionShape, conv: Conv): PositionGeometry | null {
  const top = shape.tp === null ? Math.max(shape.entry, shape.sl) : Math.max(shape.entry, shape.sl, shape.tp);
  const bottom =
    shape.tp === null ? Math.min(shape.entry, shape.sl) : Math.min(shape.entry, shape.sl, shape.tp);
  const x1 = conv.x(shape.t1);
  const x2 = conv.x(shape.t2);
  const y1 = conv.y(top);
  const y2 = conv.y(bottom);
  if (x1 === null || x2 === null || y1 === null || y2 === null) return null;
  const box = toBox(x1, y1, x2, y2);
  if (!(box.width > 0) && !(box.height > 0)) return null;
  const bands =
    shape.tp === null
      ? [{ from: 0, to: 1, cls: "sl" as const }]
      : positionBands(top, bottom, shape.entry, shape.sl, shape.tp);
  return { ...box, topPrice: top, bottomPrice: bottom, bands };
}

/** `LONG 1.23R · 12.5 BTC ≈ $500` — the header chip of the box. */
export function positionLabel(
  shape: PositionShape,
  positionSizeUsdt: number,
  symbol: string,
): string {
  const dir = positionDirection(shape.entry, shape.sl);
  const tp = shape.tp ?? shape.sl;
  const rr = riskReward(shape.entry, shape.sl, tp);
  const qty = positionSizeUsdt / shape.entry;
  const base = symbol.split("/")[0] ?? "";
  return (
    `${dir === "long" ? "LONG" : "SHORT"} ${rr === null ? "—" : rr.toFixed(2) + "R"}` +
    ` · ${qty.toPrecision(4)} ${base} ≈ $${positionSizeUsdt}`
  );
}

export function drawPosition(
  scope: Scope,
  g: PositionGeometry,
  shape: PositionShape,
  options: { color: string; selected: boolean; label?: string | null; k?: number },
): void {
  const k = options.k ?? 1;
  const { color, selected } = options;
  for (const band of g.bands) {
    if (band.cls === "entry") continue;
    const top = g.top + band.from * g.height;
    const height = (band.to - band.from) * g.height;
    fillRectBox(
      scope,
      g.left,
      top,
      g.width,
      height,
      alpha(band.cls === "tp" ? C.green : C.red, 0.18 * k),
    );
  }
  const entryY = g.top + (g.bands.find((b) => b.cls === "entry")?.from ?? 0.5) * g.height;
  strokeSegment(
    scope,
    g.left,
    entryY,
    g.left + g.width,
    entryY,
    alpha(C.accent, k),
    1,
    LineStyle.Dashed,
  );
  strokeRectBox(
    scope,
    g.left,
    g.top,
    g.width,
    g.height,
    k < 1 ? alpha(color, k) : selected ? brighten(color) : color,
    1,
    selected ? LineStyle.Solid : LineStyle.Dashed,
  );
  if (options.label) {
    drawChip(scope, options.label, g.left + 6, g.top + 4, {
      background: k < 1 ? alpha(C.chip, k) : C.chip,
      foreground: C.text,
      fontSize: 11,
      weight: "700",
      radius: 5,
      padX: 7,
      padY: 1,
    });
  }
}

export class PositionPrimitive extends DrawingPrimitive<"position"> {
  public readonly kind = "position";

  private readonly _labels: ISeriesPrimitiveAxisView[];

  public constructor(host: PrimitiveHost) {
    super(host);
    const label = (pick: (d: Position) => number, color: string): AxisLabelSource => ({
      coordinate: (): Coordinate | null =>
        this._d && this._conv ? this._conv.y(pick(this._d)) : null,
      text: () => (this._d ? this.series.priceFormatter().format(pick(this._d)) : ""),
      textColor: () => C.bg,
      backColor: () => color,
      tickVisible: () => false,
    });
    this._labels = [
      new AxisLabelView(label((d) => d.entry, C.accent)),
      new AxisLabelView(label((d) => d.sl, C.red)),
      new AxisLabelView(label((d) => d.tp, C.green)),
    ];
  }

  public priceAxisViews(): readonly ISeriesPrimitiveAxisView[] {
    return this._labels;
  }

  protected drawPane(target: CanvasRenderingTarget2D, conv: Conv): void {
    const d = this._d;
    if (!d) return;
    const shape: PositionShape = { t1: d.t1, t2: d.t2, entry: d.entry, sl: d.sl, tp: d.tp };
    const g = positionGeometry(shape, conv);
    if (!g) return;
    const label = positionLabel(shape, this._host.positionSizeUsdt, this._host.symbol);
    target.useBitmapCoordinateSpace((scope) =>
      drawPosition(scope, g, shape, {
        color: d.color,
        selected: this._selected,
        label,
      }),
    );
  }

  protected hit(x: number, y: number): HitResult | null {
    const d = this._d;
    if (!d || !this._conv) return null;
    const g = positionGeometry(
      { t1: d.t1, t2: d.t2, entry: d.entry, sl: d.sl, tp: d.tp },
      this._conv,
    );
    if (!g) return null;
    const distance = distanceToRect(x, y, g.left, g.top, g.width, g.height);
    if (distance > 6) return null;
    return { distance, priority: 0, cursor: "move" };
  }
}
