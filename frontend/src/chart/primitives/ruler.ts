/**
 * Ruler (two clicks): a translucent measurement box with the readout chip
 * above it. Transient by design — it fades out and is restored on the next
 * plain click, so it carries no store state.
 */
import { LineStyle } from "@tradingview/lwc-toolkit/line-style";
import type { CanvasRenderingTarget2D } from "fancy-canvas";
import { countBars, formatRuler, rulerStats, toBox } from "../overlays";
import { TransientPrimitive, type PrimitiveHost } from "./base";
import { C, alpha, drawChip, fillRectBox, strokeRectBox, type Conv, type Pt, type Scope } from "./core";

export interface RulerShape {
  a: Pt;
  b: Pt;
}

export interface RulerGeometry {
  left: number;
  top: number;
  width: number;
  height: number;
}

export function rulerGeometry(shape: RulerShape, conv: Conv): RulerGeometry | null {
  const x1 = conv.x(shape.a.t);
  const x2 = conv.x(shape.b.t);
  const y1 = conv.y(shape.a.p);
  const y2 = conv.y(shape.b.p);
  if (x1 === null || x2 === null || y1 === null || y2 === null) return null;
  const box = toBox(x1, y1, x2, y2);
  if (!(box.width > 0) && !(box.height > 0)) return null;
  return box;
}

/** `+1.24% · 12 bars · +0.003 pts`, computed from this pane's own candles. */
export function rulerText(shape: RulerShape, times: number[]): string {
  const stats = rulerStats(shape.a, shape.b, 60000);
  const bars = countBars(times, shape.a.t, shape.b.t);
  return formatRuler({ ...stats, bars });
}

export function drawRuler(scope: Scope, g: RulerGeometry, text: string, k = 1): void {
  fillRectBox(scope, g.left, g.top, g.width, g.height, alpha(C.accent, 0.12 * k));
  strokeRectBox(scope, g.left, g.top, g.width, g.height, alpha(C.accent, k), 2, LineStyle.Solid);
  drawChip(scope, text, g.left + g.width / 2, g.top - 3, {
    background: k < 1 ? alpha("rgba(11, 14, 19, 0.8)", k) : "rgba(11, 14, 19, 0.8)",
    foreground: C.accent,
    border: alpha(C.line, k),
    fontSize: 12,
    weight: "700",
    radius: 7,
    padX: 10,
    padY: 3,
    align: "center",
    vAlign: "bottom",
  });
}

export class RulerPrimitive extends TransientPrimitive {
  private _shape: RulerShape | null = null;
  private _alpha = 1;

  public setShape(shape: RulerShape | null): void {
    this._shape = shape;
    this.requestUpdate();
  }

  public fadeTo(value: number): void {
    if (this._alpha === value) return;
    this._alpha = value;
    this.requestUpdate();
  }

  public get alphaValue(): number {
    return this._alpha;
  }

  protected drawPane(target: CanvasRenderingTarget2D, conv: Conv): void {
    if (!this._shape || this._alpha <= 0.01) return;
    const g = rulerGeometry(this._shape, conv);
    if (!g) return;
    const text = rulerText(this._shape, this._host.times());
    target.useBitmapCoordinateSpace((scope) => drawRuler(scope, g, text, this._alpha));
  }
}
