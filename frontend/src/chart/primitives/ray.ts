/**
 * Horizontal level ray: a dashed line from a clicked bar to the right edge,
 * with its anchor dot and a price label on the axis (the way TradingView
 * draws a horizontal ray).
 */
import { AxisLabelView, type AxisLabelSource } from "@tradingview/lwc-toolkit/axis-label-view";
import { LineStyle } from "@tradingview/lwc-toolkit/line-style";
import type { CanvasRenderingTarget2D } from "fancy-canvas";
import type { Coordinate, ISeriesPrimitiveAxisView } from "lightweight-charts";
import type { Drawing } from "../../state/store";
import { DrawingPrimitive, type HitResult, type PrimitiveHost } from "./base";
import {
  C,
  alpha,
  brighten,
  fillCircle,
  strokeSegment,
  type Conv,
  type Scope,
} from "./core";

export type Ray = Extract<Drawing, { kind: "ray" }>;

export interface RayGeometry {
  x: number;
  y: number;
  right: number;
}

export function rayGeometry(d: Ray, conv: Conv): RayGeometry | null {
  const x = conv.x(d.t0);
  const y = conv.y(d.price);
  if (x === null || y === null) return null;
  return { x, y, right: conv.width() };
}

/** Draw the ray: 3px dashed line, anchor dot, selected = solid + halo. */
export function drawRay(
  scope: Scope,
  g: RayGeometry,
  color: string,
  selected: boolean,
): void {
  const line = selected ? brighten(color) : color;
  if (selected) {
    strokeSegment(scope, g.x, g.y, g.right, g.y, alpha(line, 0.35), 7, LineStyle.Solid);
  }
  strokeSegment(
    scope,
    g.x,
    g.y,
    g.right,
    g.y,
    line,
    3,
    selected ? LineStyle.Solid : LineStyle.Dashed,
  );
  fillCircle(scope, g.x, g.y, 4.5, line);
}

export class RayPrimitive extends DrawingPrimitive<"ray"> {
  public readonly kind = "ray";

  private readonly _label: AxisLabelView;
  private readonly _labels: ISeriesPrimitiveAxisView[];

  public constructor(host: PrimitiveHost) {
    super(host);
    const source: AxisLabelSource = {
      coordinate: (): Coordinate | null =>
        this._d && this._conv ? this._conv.y(this._d.price) : null,
      text: () => (this._d ? this.series.priceFormatter().format(this._d.price) : ""),
      textColor: () => C.bg,
      backColor: () => (this._d ? this._d.color : C.accent),
      tickVisible: () => false,
    };
    this._label = new AxisLabelView(source);
    this._labels = [this._label];
  }

  public priceAxisViews(): readonly ISeriesPrimitiveAxisView[] {
    return this._labels;
  }

  protected drawPane(target: CanvasRenderingTarget2D, conv: Conv): void {
    const d = this._d;
    if (!d) return;
    const g = rayGeometry(d, conv);
    if (!g) return;
    target.useBitmapCoordinateSpace((scope) =>
      drawRay(scope, g, d.color, this._selected),
    );
  }

  protected hit(x: number, y: number): HitResult | null {
    const d = this._d;
    if (!d || !this._conv) return null;
    const g = rayGeometry(d, this._conv);
    if (!g) return null;
    if (x < g.x - 6) return null; // the ray starts at its anchor
    const distance = Math.abs(y - g.y);
    if (distance > 6) return null;
    return { distance, priority: 1, cursor: "ns-resize" };
  }
}
