/**
 * Zone rectangle (two clicks). Rendered by the chart as a series primitive:
 * translucent fill, dashed border, and axis labels for both price edges and
 * both time edges — the way TradingView annotates a rectangle.
 */
import { AxisLabelView, type AxisLabelSource } from "@tradingview/lwc-toolkit/axis-label-view";
import { LineStyle } from "@tradingview/lwc-toolkit/line-style";
import type { CanvasRenderingTarget2D } from "fancy-canvas";
import type { Coordinate, ISeriesPrimitiveAxisView } from "lightweight-charts";
import type { Drawing } from "../../state/store";
import { toBox } from "../overlays";
import { DrawingPrimitive, type HitResult, type PrimitiveHost } from "./base";
import {
  C,
  alpha,
  brighten,
  distanceToRect,
  fillRectBox,
  hhmm,
  strokeRectBox,
  type Conv,
  type Pt,
  type Scope,
} from "./core";

export type Zone = Extract<Drawing, { kind: "zone" }>;

export interface ZoneGeometry {
  left: number;
  top: number;
  width: number;
  height: number;
}

/** Normalise two clicks into a pixel box (any click order). */
export function zoneGeometry(a: Pt, b: Pt, conv: Conv): ZoneGeometry | null {
  const x1 = conv.x(a.t);
  const x2 = conv.x(b.t);
  const y1 = conv.y(a.p);
  const y2 = conv.y(b.p);
  if (x1 === null || x2 === null || y1 === null || y2 === null) return null;
  const box = toBox(x1, y1, x2, y2);
  if (!(box.width > 0) && !(box.height > 0)) return null;
  return box;
}

/** Fill + dashed border; `k` fades the whole shape for ghost previews. */
export function drawZone(
  scope: Scope,
  g: ZoneGeometry,
  color: string,
  selected: boolean,
  k = 1,
): void {
  fillRectBox(scope, g.left, g.top, g.width, g.height, alpha(color, 0.1 * k));
  strokeRectBox(
    scope,
    g.left,
    g.top,
    g.width,
    g.height,
    k < 1 ? alpha(color, k) : selected ? brighten(color) : color,
    2,
    selected ? LineStyle.Solid : LineStyle.Dashed,
  );
}

export class ZonePrimitive extends DrawingPrimitive<"zone"> {
  public readonly kind = "zone";

  private readonly _priceLabels: ISeriesPrimitiveAxisView[];
  private readonly _timeLabels: ISeriesPrimitiveAxisView[];

  public constructor(host: PrimitiveHost) {
    super(host);
    const price = (pick: (d: Zone) => number, color: (d: Zone) => string): AxisLabelSource => ({
      coordinate: (): Coordinate | null =>
        this._d && this._conv ? this._conv.y(pick(this._d)) : null,
      text: () => (this._d ? this.series.priceFormatter().format(pick(this._d)) : ""),
      textColor: () => C.bg,
      backColor: () => (this._d ? color(this._d) : C.accent),
      tickVisible: () => false,
    });
    const time = (pick: (d: Zone) => number, color: (d: Zone) => string): AxisLabelSource => ({
      coordinate: (): Coordinate | null =>
        this._d && this._conv ? this._conv.x(pick(this._d)) : null,
      text: () => (this._d ? hhmm(pick(this._d)) : ""),
      textColor: () => C.bg,
      backColor: () => (this._d ? color(this._d) : C.accent),
    });
    this._priceLabels = [
      new AxisLabelView(price((d) => Math.max(d.p1, d.p2), (d) => d.color)),
      new AxisLabelView(price((d) => Math.min(d.p1, d.p2), (d) => d.color)),
    ];
    this._timeLabels = [
      new AxisLabelView(time((d) => d.t1, (d) => d.color)),
      new AxisLabelView(time((d) => d.t2, (d) => d.color)),
    ];
  }

  public priceAxisViews(): readonly ISeriesPrimitiveAxisView[] {
    return this._priceLabels;
  }

  public timeAxisViews(): readonly ISeriesPrimitiveAxisView[] {
    return this._timeLabels;
  }

  protected drawPane(target: CanvasRenderingTarget2D, conv: Conv): void {
    const d = this._d;
    if (!d) return;
    const g = zoneGeometry({ t: d.t1, p: d.p1 }, { t: d.t2, p: d.p2 }, conv);
    if (!g) return;
    target.useBitmapCoordinateSpace((scope) =>
      drawZone(scope, g, d.color, this._selected),
    );
  }

  protected hit(x: number, y: number): HitResult | null {
    const d = this._d;
    if (!d || !this._conv) return null;
    const g = zoneGeometry({ t: d.t1, p: d.p1 }, { t: d.t2, p: d.p2 }, this._conv);
    if (!g) return null;
    const distance = distanceToRect(x, y, g.left, g.top, g.width, g.height);
    if (distance > 6) return null;
    return { distance, priority: 0, cursor: "move" };
  }
}
