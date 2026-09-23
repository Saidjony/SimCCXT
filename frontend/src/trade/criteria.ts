// Setup checklist: 30 hand-picked criteria. The sidebar card toggles them;
// selected ids render as chips in the chart's top-left overlay (Chart.tsx).
// Labels are short on purpose — the overlay has room for ~8.

export interface Criterion {
  id: string;
  label: string;
}

export const CRITERIA: Criterion[] = [
  { id: "level", label: "Уровень" },
  { id: "accum", label: "Накопление" },
  { id: "squeeze", label: "Поджатие" },
  { id: "stick", label: "Прилипание" },
  { id: "atr", label: "ATR" },
  { id: "ignore-spike", label: "Игнор импульса" },
  { id: "trend", label: "Тренд" },
  { id: "ht-lt", label: "ГТ/ЛТ" },
  { id: "close-at-level", label: "Закрытие у уровня" },
  { id: "limit-player", label: "Лимитник" },
  { id: "break-no-move", label: "Пробой без движения" },
  { id: "retest", label: "Ретест" },
  { id: "v-shape", label: "V-формация" },
  { id: "small-bars-ltf", label: "Мелкие бары на МТФ" },
  { id: "room", label: "Запас хода" },
  { id: "extremum", label: "Экстремум" },
  { id: "clean-zone", label: "Чистая зона" },
  { id: "pressure-2", label: "Давление ×2" },
  { id: "round", label: "Круглый уровень" },
  { id: "buyout", label: "Выкуп" },
  { id: "weak-vs-market", label: "Слабее рынка" },
  { id: "near-retest", label: "Ближний ретест" },
  { id: "vol-squeeze", label: "Сужение волы" },
  { id: "yank", label: "Задёрг" },
  { id: "engulf", label: "Поглощение" },
  { id: "fast-pullback", label: "Быстрый откат" },
  { id: "ipo", label: "IPO" },
  { id: "hvol", label: "Гориз. объём" },
  { id: "levels-near", label: "Уровни рядом" },
  { id: "enter-at-level", label: "Вход ровно на уровне" },
];

/** Label lookup for overlay chips; unknown ids render as-is. */
export function criterionLabel(id: string): string {
  return CRITERIA.find((c) => c.id === id)?.label ?? id;
}
