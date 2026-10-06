/**
 * Live preview of the drawing being placed (the "ghost"): zone, position and
 * ruler shapes follow the cursor until the last click lands. It is a chart
 * primitive like every other drawing — it just has no store state and no hit
 * test, so it can never swallow the click that finishes it.
 */
import type { CanvasRenderingTarget2D } from "fancy-canvas";
import { positionDirection } from "../overlays";
import { TransientPrimitive, type PrimitiveHost } from "./base";
import { type Conv, type Pt } from "./core";
import { drawPosition, positionGeometry, positionLabel, type PositionShape } from "./position";
import { drawRuler, rulerGeometry, rulerText, type RulerShape } from "./ruler";
import { drawZone, zoneGeometry } from "./zone";

export type GhostShape =
  | { kind: "zone"; a: Pt; b: Pt }
  | { kind: "ruler"; a: Pt; b: Pt }
  | { kind: "position"; entry: Pt; sl: Pt | null; tp: Pt | null };

const GHOST_ALPHA = 0.55;

export class GhostPrimitive extends TransientPrimitive {
  private _shape: GhostShape | null = null;

  public setShape(shape: GhostShape | null): void {
    this._shape = shape;
    this.requestUpdate();
  }

  protected drawPane(target: CanvasRenderingTarget2D, conv: Conv): void {
    const shape = this._shape;
    if (!shape) return;
    if (shape.kind === "zone") {
      const g = zoneGeometry(shape.a, shape.b, conv);
      if (!g) return;
      target.useBitmapCoordinateSpace((scope) =>
        drawZone(scope, g, "#4a9ec4", false, GHOST_ALPHA),
      );
      return;
    }
    if (shape.kind === "ruler") {
      const g: RulerShape = { a: shape.a, b: shape.b };
      const box = rulerGeometry(g, conv);
      if (!box) return;
      const text = rulerText(g, this._host.times());
      target.useBitmapCoordinateSpace((scope) => drawRuler(scope, box, text));
      return;
    }
    if (!shape.sl) return;
    const last = shape.tp ?? shape.sl;
    const s: PositionShape = {
      t1: shape.entry.t,
      t2: last.t,
      entry: shape.entry.p,
      sl: shape.sl.p,
      tp: shape.tp === null ? null : shape.tp.p,
    };
    const g = positionGeometry(s, conv);
    if (!g) return;
    const dir = positionDirection(s.entry, s.sl);
    const label =
      s.tp === null
        ? null
        : positionLabel(s, this._host.positionSizeUsdt, this._host.symbol);
    target.useBitmapCoordinateSpace((scope) =>
      drawPosition(
        scope,
        g,
        s,
        {
          color: dir === "long" ? "#3d9a50" : "#d46666",
          selected: false,
          label,
          k: GHOST_ALPHA,
        },
      ),
    );
  }
}
