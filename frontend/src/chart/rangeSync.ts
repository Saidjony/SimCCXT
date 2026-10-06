/**
 * Follow-the-latest behaviour for the main chart.
 *
 * There is nothing left to synchronize across charts: OI and CVD are panes of
 * the main chart (same time scale by construction), and the mini charts only
 * mirror the main chart's crosshair (see crosshairSync.ts).
 */
export function followLatest(enabled: boolean, scrollToLatest: () => void): void {
  if (enabled) scrollToLatest();
}
