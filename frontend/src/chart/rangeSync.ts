/**
 * Follow-the-latest behaviour for the main chart.
 *
 * The mini charts only mirror the main chart's crosshair (see
 * crosshairSync.ts); there is no second time scale to synchronize.
 */
export function followLatest(enabled: boolean, scrollToLatest: () => void): void {
  if (enabled) scrollToLatest();
}
