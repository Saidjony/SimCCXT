/**
 * Base classes for chart primitives.
 *
 * `ChartPrimitive` owns the attachment plumbing (chart + series refs, the
 * coordinate bridge, a stable pane view). From it:
 *   - `DrawingPrimitive`  — a persisted store drawing (select / hover / drag);
 *   - `TransientPrimitive` — a preview or measurement with no interaction.
 *
 * Every primitive is drawn by lightweight-charts inside its own canvas; there
 * is no overlay DOM anywhere in this layer.
 */
import { PluginBase } from "@tradingview/lwc-toolkit/plugin-base";
import type { CanvasRenderingTarget2D } from "fancy-canvas";
import type {
  IPrimitivePaneView,
  ISeriesPrimitive,
  PrimitiveHoveredItem,
  SeriesAttachedParameter,
  Time,
} from "lightweight-charts";
import type { Drawing } from "../../state/store";
import { makeConv, makePaneView, type Conv } from "./core";

/** Cross-drawing context shared by every primitive on one chart. */
export interface PrimitiveHost {
  /** Bar timestamps of this pane: drives nearest-bar snapping of drawings. */
  times: () => number[];
  /** Order ticket size (USDT) and symbol — the position label shows them. */
  positionSizeUsdt: number;
  symbol: string;
}

export interface HitResult {
  /** Distance from the cursor to the shape, in CSS pixels. */
  distance: number;
  /** Hit priority: 0 areas, 1 strokes, 2 points (per the library's guidance). */
  priority: number;
  /** Cursor to show while hovering. */
  cursor?: string;
}

export abstract class ChartPrimitive extends PluginBase {
  protected readonly _host: PrimitiveHost;
  protected _conv: Conv | null = null;

  private readonly _paneViews: IPrimitivePaneView[];
  private readonly _paneRenderer = {
    draw: (target: CanvasRenderingTarget2D): void => {
      if (this._conv === null) return;
      this.drawPane(target, this._conv);
    },
  };

  public constructor(host: PrimitiveHost) {
    super();
    this._host = host;
    this._paneViews = [makePaneView(() => this._paneRenderer)];
  }

  public override attached(param: SeriesAttachedParameter<Time>): void {
    super.attached(param);
    this._conv = makeConv(this.chart, this.series, this._host.times);
    this.requestUpdate();
  }

  public override detached(): void {
    this._conv = null;
    super.detached();
  }

  public paneViews(): readonly IPrimitivePaneView[] {
    return this._paneViews;
  }

  /** Repaint after something outside the primitive (sizing, symbol) changed. */
  public refresh(): void {
    this.requestUpdate();
  }

  /** Draw into the pane canvas. Coordinates come from the chart, always. */
  protected abstract drawPane(target: CanvasRenderingTarget2D, conv: Conv): void;
}

export abstract class DrawingPrimitive<K extends Drawing["kind"]> extends ChartPrimitive {
  public abstract readonly kind: K;

  protected _d: Extract<Drawing, { kind: K }> | null = null;
  protected _selected = false;
  /** Ghosts (in-progress previews) are drawn but never hit-testable. */
  protected _ghost = false;
  /** False while a drawing tool is armed: placement wins over selection. */
  protected _hitTestable = true;

  /** Store drawing id — also the `externalId` the chart reports on hover. */
  public get drawingId(): string | null {
    return this._d?.id ?? null;
  }

  /** The object the chart attaches to the series (this primitive itself). */
  public get target(): ISeriesPrimitive<Time> {
    return this;
  }

  /** Push a store drawing into the primitive and repaint. */
  public setDrawing(d: Drawing): void {
    if (d.kind !== this.kind) return;
    this._d = d as Extract<Drawing, { kind: K }>;
    this.requestUpdate();
  }

  public setSelected(selected: boolean): void {
    if (this._selected === selected) return;
    this._selected = selected;
    this.requestUpdate();
  }

  public setGhost(ghost: boolean): void {
    if (this._ghost === ghost) return;
    this._ghost = ghost;
    this.requestUpdate();
  }

  public setHitTestable(value: boolean): void {
    if (this._hitTestable === value) return;
    this._hitTestable = value;
    this.requestUpdate();
  }

  public hitTest(x: number, y: number): PrimitiveHoveredItem | null {
    if (this._ghost || !this._hitTestable || this._d === null || this._conv === null) {
      return null;
    }
    const hit = this.hit(x, y);
    if (hit === null) return null;
    return {
      externalId: this._d.id,
      zOrder: "top",
      distance: hit.distance,
      hitTestPriority: hit.priority,
      cursorStyle: hit.cursor,
    };
  }

  /** Hit test in pane coordinates (CSS pixels, same space as priceToCoordinate). */
  protected abstract hit(x: number, y: number): HitResult | null;
}

/** A preview or measurement: rendered by the chart, never interactive. */
export abstract class TransientPrimitive extends ChartPrimitive {}
