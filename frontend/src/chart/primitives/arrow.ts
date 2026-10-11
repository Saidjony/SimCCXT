/** Arrow tool: 2-click directional arrow from A to B. */
import type { CanvasRenderingTarget2D } from "fancy-canvas";
import type { Drawing } from "../../state/store";
import { DrawingPrimitive, type HitResult } from "./base";
import { C, alpha, type Conv } from "./core";

export type Arrow = Extract<Drawing, { kind: "arrow" }>;

export class ArrowPrimitive extends DrawingPrimitive<"arrow"> {
  public readonly kind = "arrow";

  protected drawPane(target: CanvasRenderingTarget2D, conv: Conv): void {
    const d = this._d;
    if (!d) return;
    const x0 = conv.x(d.t0);
    const y0 = conv.y(d.price);
    const x1 = conv.x(d.t1);
    const y1 = conv.y(d.price1);
    if (x0 === null || y0 === null || x1 === null || y1 === null) return;
    target.useBitmapCoordinateSpace((scope) => {
      const i = scope.context;
      i.strokeStyle = this._selected ? C.accent : d.color;
      i.lineWidth = this._selected ? 3 : 2;
      i.beginPath();
      i.moveTo(x0, y0);
      i.lineTo(x1, y1);
      i.stroke();
      // Simple arrowhead at the end
      const angle = Math.atan2(y1 - y0, x1 - x0);
      const len = 12;
      i.beginPath();
      i.moveTo(x1, y1);
      i.lineTo(x1 - len * Math.cos(angle - 0.5), y1 - len * Math.sin(angle - 0.5));
      i.moveTo(x1, y1);
      i.lineTo(x1 - len * Math.cos(angle + 0.5), y1 - len * Math.sin(angle + 0.5));
      i.stroke();
    });
  }

  protected hit(x: number, y: number): HitResult | null {
    const d = this._d;
    if (!d || !this._conv) return null;
    const x0 = this._conv.x(d.t0);
    const y0 = this._conv.y(d.price);
    const x1 = this._conv.x(d.t1);
    const y1 = this._conv.y(d.price1);
    if (x0 === null || y0 === null || x1 === null || y1 === null) return null;
    // Approximate: distance to line segment, prioritize nearby
    const t = Math.max(0, Math.min(1, ((x - x0) * (x1 - x0) + (y - y0) * (y1 - y0)) / ((x1 - x0) * (x1 - x0) + (y1 - y0) * (y1 - y0) || 1)));
    const px = x0 + t * (x1 - x0);
    const py = y0 + t * (y1 - y0);
    const dist = Math.hypot(x - px, y - py);
    if (dist > 6) return null;
    return { distance: dist, priority: 1, cursor: "move" };
  }
}
