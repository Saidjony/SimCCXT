/**
 * Fixed-range volume profile of the bars currently on screen, anchored to the
 * right edge of the pane, POC flagged. The math (`volumeProfile`,
 * `profileBarLayout`) is unchanged and unit-tested; only the rendering moved
 * from an HTML strip into the chart canvas.
 */
import type { CanvasRenderingTarget2D } from "fancy-canvas";
import { profileBarLayout, volumeProfile, type OhlcRow } from "../overlays";
import { TransientPrimitive, type PrimitiveHost } from "./base";
import { C, alpha, drawChip, fillRectBox, type Conv } from "./core";

export const PROFILE_LEVELS = 64;
const PROFILE_WIDTH = 150;
const PROFILE_GAP = 8;

export class VolumeProfilePrimitive extends TransientPrimitive {
  private _enabled = false;
  private _rows: () => OhlcRow[] = () => [];

  public constructor(host: PrimitiveHost) {
    super(host);
  }

  public setEnabled(enabled: boolean): void {
    if (this._enabled === enabled) return;
    this._enabled = enabled;
    this.requestUpdate();
  }

  /** The candle buffer this profile is built from (shared with the series). */
  public setRows(rows: () => OhlcRow[]): void {
    this._rows = rows;
    this.requestUpdate();
  }

  protected drawPane(target: CanvasRenderingTarget2D, conv: Conv): void {
    if (!this._enabled) return;
    const rows = this._rows();
    if (rows.length < 2) return;
    const range = this.chart.timeScale().getVisibleLogicalRange();
    const { rows: levels, poc } = volumeProfile(
      rows,
      range ? range.from : 0,
      range ? range.to : rows.length - 1,
      PROFILE_LEVELS,
    );
    if (!levels.length || poc === null) return;
    const maxVolume = levels.reduce((m, r) => Math.max(m, r.volume), 0);
    if (!(maxVolume > 0)) return;
    const layout = profileBarLayout(
      levels.map((level) => level.price),
      (price) => conv.y(price),
      conv.height(),
    );
    const right = conv.width() - PROFILE_GAP;
    target.useBitmapCoordinateSpace((scope) => {
      levels.forEach((level, index) => {
        const position = layout[index];
        if (!position) return;
        const width = (level.volume / maxVolume) * PROFILE_WIDTH;
        if (!(width > 0)) return;
        fillRectBox(
          scope,
          right - width,
          position.top,
          width,
          position.height,
          level.poc ? alpha(C.yellow, 0.55) : alpha(C.accent, 0.35),
        );
        if (!level.poc) return;
        drawChip(scope, level.price.toLocaleString("en-US", { maximumFractionDigits: 1 }), right - 4, position.top + position.height / 2, {
          background: "rgba(11, 14, 19, 0.8)",
          foreground: C.yellow,
          fontSize: 10,
          weight: "700",
          radius: 4,
          padX: 5,
          padY: 1,
          align: "right",
          vAlign: "middle",
        });
      });
    });
  }
}
