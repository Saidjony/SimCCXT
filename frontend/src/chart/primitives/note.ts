/**
 * Text note (one click). A chip anchored to a bar/price, painted in the
 * chart canvas so it pans and zooms with the bars it annotates.
 */
import type { CanvasRenderingTarget2D } from "fancy-canvas";
import type { Drawing } from "../../state/store";
import { DrawingPrimitive, type HitResult, type PrimitiveHost } from "./base";
import {
  C,
  alpha,
  chipBounds,
  drawChip,
  distanceToRect,
  type ChipOptions,
  type Conv,
} from "./core";

export type Note = Extract<Drawing, { kind: "note" }>;

export const NOTE_CHIP: ChipOptions = {
  background: alpha(C.yellow, 0.18),
  foreground: C.text,
  border: C.yellow,
  fontSize: 11,
  radius: 4,
  padX: 6,
  padY: 2,
};

/** Notes hang 8px below their anchor, exactly like the old caption. */
export const NOTE_OFFSET = 8;

export class NotePrimitive extends DrawingPrimitive<"note"> {
  public readonly kind = "note";

  protected drawPane(target: CanvasRenderingTarget2D, conv: Conv): void {
    const d = this._d;
    if (!d) return;
    const x = conv.x(d.t0);
    const y = conv.y(d.price);
    if (x === null || y === null) return;
    target.useBitmapCoordinateSpace((scope) =>
      drawChip(scope, d.title, x, y + 8, {
        ...NOTE_CHIP,
        border: this._selected ? C.accent : NOTE_CHIP.border,
      }),
    );
  }

  protected hit(x: number, y: number): HitResult | null {
    const d = this._d;
    if (!d || !this._conv) return null;
    const ax = this._conv.x(d.t0);
    const ay = this._conv.y(d.price);
    if (ax === null || ay === null) return null;
    const box = chipBounds(d.title, ax, ay + 8, NOTE_CHIP);
    const distance = distanceToRect(x, y, box.left, box.top, box.width, box.height);
    if (distance > 6) return null;
    return { distance, priority: 2, cursor: "move" };
  }
}
