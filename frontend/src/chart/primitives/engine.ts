/**
 * Owns every drawing-related primitive on one chart: keeps them in sync with
 * the store, holds the transient layers (ghost preview, ruler, volume profile,
 * execution lines) and reports which drawing the cursor is over.
 *
 * The chart component never touches coordinates — it feeds the engine store
 * state and interaction intent; the engine feeds back hit information.
 */
import type {
  IChartApi,
  ISeriesApi,
  ISeriesPrimitive,
  MouseEventParams,
  SeriesOptionsMap,
  Time,
} from "lightweight-charts";
import type { Drawing } from "../../state/store";
import type { OhlcRow } from "../overlays";
import { BarrierPrimitive } from "./barrier";
import type { PrimitiveHost } from "./base";
import { DrawingPrimitive } from "./base";
import { NotePrimitive } from "./note";
import { PositionPrimitive } from "./position";
import { RayPrimitive } from "./ray";
import { RulerPrimitive, type RulerShape } from "./ruler";
import { ZonePrimitive } from "./zone";
import { GhostPrimitive, type GhostShape } from "./ghost";
import { ExecutionPrimitive, type ExecutionData } from "./execution";
import { VolumeProfilePrimitive } from "./volumeProfile";

/** Anything the engine can attach to the series and drive from the store. */
interface Managed {
  readonly target: ISeriesPrimitive<Time>;
  setDrawing(d: Drawing): void;
  setSelected(selected: boolean): void;
  setHitTestable(value: boolean): void;
  refresh(): void;
}

export interface EngineOptions {
  chart: IChartApi;
  series: ISeriesApi<keyof SeriesOptionsMap>;
  /** Bar timestamps of this pane. */
  times: () => number[];
  /** Order ticket size (USDT) and symbol, for the position header. */
  positionSizeUsdt: number;
  symbol: string;
  /** Track hovered drawings (the interactive chart only). */
  trackHover?: boolean;
}

export class DrawingEngine {
  private readonly _series: ISeriesApi<keyof SeriesOptionsMap>;
  private readonly _host: PrimitiveHost;
  private readonly _items = new Map<string, Managed>();

  private _armed = false;
  private _hoveredId: string | null = null;
  private _unsubscribeHover: (() => void) | null = null;

  private _ghost: GhostPrimitive | null = null;
  private _ruler: RulerPrimitive | null = null;
  private _profile: VolumeProfilePrimitive | null = null;
  private _execution: ExecutionPrimitive | null = null;
  private _fadeFrame = 0;

  private readonly _context = { positionSizeUsdt: 0, symbol: "" };

  public constructor(options: EngineOptions) {
    this._series = options.series;
    this._context.positionSizeUsdt = options.positionSizeUsdt;
    this._context.symbol = options.symbol;
    this._host = {
      times: options.times,
      positionSizeUsdt: options.positionSizeUsdt,
      symbol: options.symbol,
    };
    if (options.trackHover) {
      const listener = (param: MouseEventParams): void => {
        const id = param.hoveredInfo?.objectId;
        this._hoveredId = typeof id === "string" && id.length > 0 ? id : null;
      };
      options.chart.subscribeCrosshairMove(listener);
      this._unsubscribeHover = () => options.chart.unsubscribeCrosshairMove(listener);
    }
  }

  /** Store id of the drawing under the cursor, if any. */
  public hoveredId(): string | null {
    return this._hoveredId;
  }

  /** Reconcile attached primitives with the store's drawing list. */
  public sync(drawings: Drawing[], selectedId: string | null): void {
    const seen = new Set<string>();
    for (const drawing of drawings) {
      seen.add(drawing.id);
      let item = this._items.get(drawing.id);
      if (!item) {
        item = this._create(drawing);
        this._series.attachPrimitive(item.target);
        this._items.set(drawing.id, item);
      }
      item.setDrawing(drawing);
      item.setSelected(drawing.id === selectedId);
      item.setHitTestable(!this._armed);
      item.refresh();
    }
    for (const [id, item] of [...this._items.entries()]) {
      if (seen.has(id)) continue;
      this._series.detachPrimitive(item.target);
      this._items.delete(id);
      if (this._hoveredId === id) this._hoveredId = null;
    }
  }

  /**
   * While a tool is armed, drawings must not swallow the click that places the
   * next one — placement wins over selection, like in TradingView.
   */
  public setArmed(armed: boolean): void {
    if (this._armed === armed) return;
    this._armed = armed;
    for (const item of this._items.values()) item.setHitTestable(!armed);
  }

  /** Order-ticket context for the position header; repaints affected drawings. */
  public setContext(context: { positionSizeUsdt: number; symbol: string }): void {
    if (
      context.positionSizeUsdt === this._context.positionSizeUsdt &&
      context.symbol === this._context.symbol
    ) {
      return;
    }
    this._context.positionSizeUsdt = context.positionSizeUsdt;
    this._context.symbol = context.symbol;
    this._host.positionSizeUsdt = context.positionSizeUsdt;
    this._host.symbol = context.symbol;
    for (const item of this._items.values()) item.refresh();
    this._ghost?.refresh();
  }

  /** Live preview of the in-progress drawing. */
  public setGhost(shape: GhostShape | null): void {
    if (!this._ghost) {
      this._ghost = new GhostPrimitive(this._host);
      this._series.attachPrimitive(this._ghost);
    }
    this._ghost.setShape(shape);
  }

  /** The transient measurement box (null hides it, alpha resets to 1). */
  public setRuler(shape: RulerShape | null): void {
    this._cancelFade();
    if (!this._ruler) {
      this._ruler = new RulerPrimitive(this._host);
      this._series.attachPrimitive(this._ruler);
    }
    this._ruler.fadeTo(1);
    this._ruler.setShape(shape);
  }

  /** Fade the placed ruler out over `duration` ms (the CSS fade, in canvas). */
  public fadeRuler(duration: number): void {
    this._cancelFade();
    const ruler = this._ruler;
    if (!ruler) return;
    const started = performance.now();
    const step = (): void => {
      const t = Math.min(1, (performance.now() - started) / duration);
      ruler.fadeTo(1 - t);
      if (t < 1) this._fadeFrame = window.requestAnimationFrame(step);
      else this._fadeFrame = 0;
    };
    this._fadeFrame = window.requestAnimationFrame(step);
  }

  /** Volume profile of the visible bars (the VPS toggle turns it on/off). */
  public setProfile(enabled: boolean, rows: () => OhlcRow[]): void {
    if (!this._profile) {
      this._profile = new VolumeProfilePrimitive(this._host);
      this._profile.setRows(rows);
      this._series.attachPrimitive(this._profile);
    }
    this._profile.setEnabled(enabled);
  }

  /** Entry/stop lines, entry anchors and closed-trade legs. */
  public setExecution(data: ExecutionData | null): void {
    if (!this._execution) {
      this._execution = new ExecutionPrimitive(this._host);
      this._series.attachPrimitive(this._execution);
    }
    this._execution.setData(data);
  }

  public destroy(): void {
    this._cancelFade();
    this._unsubscribeHover?.();
    this._unsubscribeHover = null;
    for (const item of this._items.values()) this._series.detachPrimitive(item.target);
    this._items.clear();
    if (this._ghost) this._series.detachPrimitive(this._ghost);
    if (this._ruler) this._series.detachPrimitive(this._ruler);
    if (this._profile) this._series.detachPrimitive(this._profile);
    if (this._execution) this._series.detachPrimitive(this._execution);
    this._ghost = null;
    this._ruler = null;
    this._profile = null;
    this._execution = null;
    this._hoveredId = null;
  }

  private _cancelFade(): void {
    if (this._fadeFrame) {
      window.cancelAnimationFrame(this._fadeFrame);
      this._fadeFrame = 0;
    }
  }

  private _create(drawing: Drawing): Managed {
    switch (drawing.kind) {
      case "ray":
        return new RayPrimitive(this._host);
      case "zone":
        return new ZonePrimitive(this._host);
      case "position":
        return new PositionPrimitive(this._host);
      case "note":
        return new NotePrimitive(this._host);
      case "vline":
        return new BarrierPrimitive(drawing);
    }
  }
}

/** Convenience type for components building ghost shapes. */
export type { GhostShape, RulerShape, ExecutionData };
