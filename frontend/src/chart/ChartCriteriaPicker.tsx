import { CRITERIA } from "../trade/criteria";
import { store, useSim } from "../state/store";

export function ChartCriteriaPicker({ panel }: { panel: string }): JSX.Element {
  const selected = useSim((s) => s.chartCriteria[panel] ?? []);

  return (
    <details className="chart-criteria">
      <summary title="Критерии для этого графика">
        Критерии · {selected.length}
      </summary>
      <div className="chart-criteria-menu">
        <div className="chart-criteria-head">
          <span>Критерии графика</span>
          {selected.length > 0 && (
            <button className="btn b-dim" onClick={() => store.clearChartCriteria(panel)}>
              Сбросить
            </button>
          )}
        </div>
        <div className="chart-criteria-list">
          {CRITERIA.map((criterion) => (
            <label key={criterion.id} className="crit-row">
              <input
                type="checkbox"
                checked={selected.includes(criterion.id)}
                onChange={() => store.toggleChartCriterion(panel, criterion.id)}
              />
              <span>{criterion.label}</span>
            </label>
          ))}
        </div>
      </div>
    </details>
  );
}
