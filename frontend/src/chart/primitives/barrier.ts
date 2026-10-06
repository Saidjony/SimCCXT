/**
 * Replay barrier — the vertical line that re-points the Rust clock.
 *
 * Rendering, time-axis label and hit testing come from the official
 * `@tradingview/lwc-plugin-vertical-line` plugin; the adapter keeps it in sync
 * with the store drawing and reuses the store id as the plugin's hit id, so
 * selection, drag and Delete behave exactly like every other drawing.
 *
 * Dragging stays on our shared path (bar magnet + "replay on drop"), which is
 * why the plugin's own `draggable` is off: one drag implementation, one snap
 * rule, one place where the clock is re-pointed.
 */
import { VerticalLine } from "@tradingview/lwc-plugin-vertical-line";
import { LineStyle } from "lightweight-charts";
import type { ISeriesPrimitive, Time } from "lightweight-charts";
import type { Drawing } from "../../state/store";
import { C, toTime } from "./core";

export type Barrier = Extract<Drawing, { kind: "vline" }>;

export class BarrierPrimitive {
  public readonly line: VerticalLine;

  private _color: string;
  private _selected = false;

  public constructor(d: Barrier) {
    this._color = d.color;
    this.line = new VerticalLine(toTime(d.t), {
      id: d.id,
      color: d.color,
      width: 3,
      lineStyle: LineStyle.Solid,
      lineVisible: true,
      showLabel: true,
      snap: "nearest",
      draggable: false,
      hitTestTolerance: 6,
      zOrder: "top",
    });
  }

  public get target(): ISeriesPrimitive<Time> {
    return this.line;
  }

  public get drawingId(): string {
    return this.line.options().id;
  }

  public setDrawing(d: Drawing): void {
    if (d.kind !== "vline") return;
    this._color = d.color;
    this.line.setTime(toTime(d.t));
    this.line.applyOptions({ color: this._selected ? C.accent : this._color });
  }

  public setSelected(selected: boolean): void {
    if (this._selected === selected) return;
    this._selected = selected;
    this.line.applyOptions({ color: selected ? C.accent : this._color });
  }

  /** Placement wins over selection while a tool is armed (handled in the chart). */
  public setHitTestable(_value: boolean): void {
    /* the official plugin owns its hit test; the chart decides what a hit means */
  }

  /** The barrier depends on nothing outside its own drawing. */
  public refresh(): void {
    /* no external context (sizing/symbol) affects a vertical line */
  }
}
