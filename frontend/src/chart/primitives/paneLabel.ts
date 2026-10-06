/**
 * Legend chip in the top-left corner of an indicator pane (OI / CVD).
 *
 * It is a series primitive on the indicator's own line series, so it lives in
 * that pane's canvas: it follows the pane wherever the chart lays it out (no
 * DOM element to position after every pane resize) and it disappears together
 * with the series. The chip style matches every other label in the terminal.
 */
import type { CanvasRenderingTarget2D } from "fancy-canvas";
import { ChartPrimitive, type PrimitiveHost } from "./base";
import { C, drawChip, type Conv } from "./core";

/** The label carries no drawing context of its own: a stub host is enough. */
const LABEL_HOST: PrimitiveHost = {
  times: () => [],
  positionSizeUsdt: 0,
  symbol: "",
};

export class PaneLabelPrimitive extends ChartPrimitive {
  private _text: string;
  private _color: string;

  public constructor(text: string, color: string) {
    super(LABEL_HOST);
    this._text = text;
    this._color = color;
  }

  /** New caption (the OI period changes with the loaded grid). */
  public setText(text: string): void {
    if (this._text === text) return;
    this._text = text;
    this.requestUpdate();
  }

  /** Line colour, so the chip reads as this series' legend. */
  public setColor(color: string): void {
    if (this._color === color) return;
    this._color = color;
    this.requestUpdate();
  }

  protected drawPane(target: CanvasRenderingTarget2D, _conv: Conv): void {
    target.useBitmapCoordinateSpace((scope) =>
      drawChip(scope, this._text, 10, 6, {
        background: C.chip,
        foreground: this._color,
        fontSize: 11,
        weight: "700",
        radius: 5,
        padX: 8,
        padY: 1,
      }),
    );
  }
}
