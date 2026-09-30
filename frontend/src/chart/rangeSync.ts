export interface VisibleRange<T> {
  from: T;
  to: T;
}

export interface RangeSyncTarget<T> {
  getRange: () => VisibleRange<T> | null;
  setRange: (range: VisibleRange<T>) => void;
}

export function sameVisibleRange<T>(
  a: VisibleRange<T> | null,
  b: VisibleRange<T> | null,
  key: (time: T) => string | number | null,
): boolean {
  if (!a || !b) return false;
  const aFrom = key(a.from);
  const aTo = key(a.to);
  const bFrom = key(b.from);
  const bTo = key(b.to);
  return aFrom !== null && aTo !== null && aFrom === bFrom && aTo === bTo;
}

/** Keep indicator charts as followers of the main chart, never as range sources. */
export function syncRangeToTargets<T>(
  source: VisibleRange<T> | null,
  targets: RangeSyncTarget<T>[],
  key: (time: T) => string | number | null,
): void {
  if (!source) return;
  for (const target of targets) {
    if (!sameVisibleRange(target.getRange(), source, key)) {
      target.setRange(source);
    }
  }
}

export function followLatest(enabled: boolean, scrollToLatest: () => void): void {
  if (enabled) scrollToLatest();
}
