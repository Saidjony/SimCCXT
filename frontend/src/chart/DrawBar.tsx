import { useEffect, useState } from "react";
import { store, useSim } from "../state/store";
import type { Tool } from "../state/store";

function RayIcon(): JSX.Element {
  return (
    <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2">
      <circle cx="4" cy="12" r="2" fill="currentColor" stroke="none" />
      <line x1="6" y1="12" x2="21" y2="12" strokeDasharray="3 2" />
    </svg>
  );
}

function VLineIcon(): JSX.Element {
  return (
    <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2">
      <line x1="12" y1="3" x2="12" y2="21" />
      <line x1="8" y1="6" x2="16" y2="6" />
      <line x1="8" y1="18" x2="16" y2="18" />
    </svg>
  );
}

function ZoneIcon(): JSX.Element {
  return (
    <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2">
      <rect x="4" y="7" width="16" height="10" strokeDasharray="3 2" />
    </svg>
  );
}

function PositionIcon(): JSX.Element {
  return (
    <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2">
      <rect x="6" y="3" width="12" height="7" fill="currentColor" opacity="0.35" stroke="none" />
      <line x1="4" y1="12" x2="20" y2="12" />
      <rect x="6" y="14" width="12" height="7" fill="currentColor" opacity="0.35" stroke="none" />
    </svg>
  );
}

function RulerIcon(): JSX.Element {
  return (
    <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2">
      <line x1="3" y1="17" x2="17" y2="3" />
      <line x1="8" y1="12" x2="10" y2="14" />
      <line x1="11" y1="9" x2="13" y2="11" />
      <line x1="14" y1="6" x2="16" y2="8" />
    </svg>
  );
}

function NoteIcon(): JSX.Element {
  return (
    <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2">
      <path d="M4 5h16v12H9l-5 4V5z" />
      <line x1="8" y1="10" x2="16" y2="10" />
      <line x1="8" y1="13" x2="13" y2="13" />
    </svg>
  );
}

function ArrowIcon(): JSX.Element {
  return (
    <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2">
      <line x1="4" y1="20" x2="18" y2="6" />
      <polyline points="18 12 18 6 12 6" />
    </svg>
  );
}

function BrushIcon(): JSX.Element {
  return (
    <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2">
      <path d="M5 19c4 0 5-4 8-7l4-4 2 2-4 4c-2 3-6 6-10 5z" />
      <line x1="14" y1="9" x2="16" y2="11" />
    </svg>
  );
}

function TrashIcon(): JSX.Element {
  return (
    <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2">
      <line x1="5" y1="7" x2="19" y2="7" />
      <line x1="9" y1="7" x2="9" y2="5" />
      <line x1="15" y1="7" x2="15" y2="5" />
      <rect x="7" y="7" width="10" height="13" />
    </svg>
  );
}

const TOOLS: { id: Exclude<Tool, "none">; title: string; Icon: () => JSX.Element }[] = [
  { id: "ray", title: "Луч-уровень (1 клик)", Icon: RayIcon },
  { id: "vline", title: "Барьер реплея (1 клик): бары справа скроются, play покажет по одному", Icon: VLineIcon },
  { id: "zone", title: "Зона-прямоугольник (2 клика)", Icon: ZoneIcon },
  { id: "position", title: "Позиция long/short + TP/SL (3 клика: вход, стоп, тейк)", Icon: PositionIcon },
  { id: "ruler", title: "Линейка (2 клика, гаснет)", Icon: RulerIcon },
  { id: "note", title: "Текстовая заметка (1 клик)", Icon: NoteIcon },
  { id: "arrow", title: "Стрелка (2 клика: от А до Б)", Icon: ArrowIcon },
  { id: "brush", title: "Ручка — свободный мазок (зажать и рисовать)", Icon: BrushIcon },
];

/** Drawing toolbar docked top-right inside the chart. Icon-only buttons. */
export function DrawBar(): JSX.Element {
  const tool = useSim((s) => s.tool);
  /** Trash is armed by the first click and fires on the second. */
  const [armed, setArmed] = useState(false);

  useEffect(() => {
    if (!armed) return;
    const timer = window.setTimeout(() => setArmed(false), 4000);
    return () => window.clearTimeout(timer);
  }, [armed]);

  const arm = (t: Exclude<Tool, "none">): void => {
    setArmed(false);
    store.setTool(tool === t ? "none" : t);
  };

  const wipe = (): void => {
    if (!armed) {
      setArmed(true);
      return;
    }
    setArmed(false);
    store.resetSymbolData();
  };

  return (
    <div className="drawbar">
      {TOOLS.map(({ id, title, Icon }) => (
        <button
          key={id}
          className={"tool-btn" + (tool === id ? " active" : "")}
          title={title}
          onClick={() => arm(id)}
        >
          <Icon />
        </button>
      ))}
      <button
        className={"tool-btn danger" + (armed ? " armed" : "")}
        title={armed
          ? "Нажмите ещё раз: удалить все сделки и рисунки этой монеты"
          : "Удалить всё по монете: рисунки, сделки, результаты"}
        onClick={wipe}
      >
        <TrashIcon />
      </button>
    </div>
  );
}
