/** Freehand brush: free-form line stroke (like Paint). Records multiple points
 *  during mouse drag and renders them as a polyline. */
import type { CanvasRenderingTarget2D } from "fancy-canvas";
import type { Drawing } from "../../state/store";
import { DrawingPrimitive, type HitResult } from "./base";
import { C, alpha, type Conv } from "./core";

export type Brush = Extract<Drawing, { kind: "brush" }>;

export class BrushPrimitive extends DrawingPrimitive<"brush"> {
  public readonly kind = "brush";

  protected drawPane(target: CanvasRenderingTarget2D, conv: Conv): void {
    const d = this._d;
    if (!d || d.points.length < 2) return;
    target.useBitmapCoordinateSpace((scope) => {
      const i = scope.context;
      i.strokeStyle = this._selected ? C.accent : d.color;
      i.lineWidth = this._selected ? 3 : 2;
      i.beginPath();
      for (let idx = 0; idx < d.points.length; idx++) {
        const pt = d.points[idx];
        const x = conv.x(pt.t);
        const y = conv.y(pt.p);
        if (x === null || y === null) continue;
        if (idx === 0) i.moveTo(x, y);
        else i.lineTo(x, y);
      }
      i.stroke();
    });
  }

  protected hit(x: number, y: number): HitResult | null {
    // Freehand selection: approximate as any point within a few px of any stroke point
    const d = this._d;
    if (!d || !this._conv) return null;
    for (let i = 0; i < d.points.length; i += Math.max(1, Math.floor(d.points.length / 20))) {
      const pt = d.points[i];
      const px = this._conv.x(pt.t);
      const py = this._conv.y(pt.p);
      if (px === null || py === null) continue;
      const dist = Math.hypot(x - px, y - py);
      if (dist <= 6) return { distance: dist, priority: 1, cursor: "move" };
    }
    return null;
  }
}
