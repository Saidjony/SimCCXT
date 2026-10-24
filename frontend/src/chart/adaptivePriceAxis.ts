import type { IChartApi, ISeriesApi } from "lightweight-charts";
import { priceAxisFormat } from "./overlays";

export function watchAdaptivePriceAxis(
  chart: IChartApi,
  series: ISeriesApi<"Bar">,
  element: HTMLElement,
  getPrice: () => number,
): { refresh: () => void; dispose: () => void } {
  let frame = 0;
  let lastFormat = "";

  const update = (): void => {
    frame = 0;
    const format = priceAxisFormat(getPrice(), series.priceScale().getVisibleRange());
    const key = `${format.precision}:${format.minMove}`;
    if (key === lastFormat) return;
    lastFormat = key;
    series.applyOptions({ priceFormat: { type: "price", ...format } });
  };
  const refresh = (): void => {
    if (frame === 0) frame = window.requestAnimationFrame(update);
  };
  const onWheel = (): void => refresh();

  element.addEventListener("pointermove", refresh, { passive: true });
  element.addEventListener("wheel", onWheel, { passive: true });
  chart.timeScale().subscribeVisibleLogicalRangeChange(refresh);
  refresh();

  return {
    refresh,
    dispose: () => {
      element.removeEventListener("pointermove", refresh);
      element.removeEventListener("wheel", onWheel);
      chart.timeScale().unsubscribeVisibleLogicalRangeChange(refresh);
      if (frame !== 0) window.cancelAnimationFrame(frame);
    },
  };
}
