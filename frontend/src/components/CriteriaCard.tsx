import { useState } from "react";
import { CRITERIA } from "../trade/criteria";
import { store, useSim } from "../state/store";

/**
 * Setup checklist: 30 criteria as checkboxes with a search filter.
 * Selected ids render as chips in the chart's top-left overlay.
 */
export function CriteriaCard(): JSX.Element {
  const criteria = useSim((s) => s.criteria);
  const [query, setQuery] = useState("");
  const q = query.trim().toLowerCase();
  const visible = q
    ? CRITERIA.filter((c) => c.label.toLowerCase().includes(q))
    : CRITERIA;

  return (
    <div className="card">
      <div className="card-head">
        <h4>Критерии</h4>
        <span className="lbl">{criteria.length} / {CRITERIA.length}</span>
      </div>
      <div className="coin-add" style={{ marginBottom: 6 }}>
        <input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="найти…"
          aria-label="найти критерий"
        />
        {criteria.length > 0 && (
          <button className="btn b-dim" onClick={() => store.clearCriteria()} title="сбросить выбор">
            ✕
          </button>
        )}
      </div>
      <div className="crit-list">
        {visible.map((c) => (
          <label key={c.id} className="crit-row">
            <input
              type="checkbox"
              checked={criteria.includes(c.id)}
              onChange={() => store.toggleCriterion(c.id)}
            />
            <span>{c.label}</span>
          </label>
        ))}
        {visible.length === 0 && <div className="lbl">ничего не найдено</div>}
      </div>
    </div>
  );
}
