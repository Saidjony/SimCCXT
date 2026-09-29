import { CRITERIA } from "../trade/criteria";
import { chartCriteriaKey, store, useSim } from "../state/store";

export function ChartCriteriaPicker({ timeframe }: { timeframe: string }): JSX.Element {
  const symbol = useSim((s) => s.symbol);
  const selected = useSim((s) => s.chartCriteria[chartCriteriaKey(s.symbol, timeframe)] ?? []);

  return (
    <details className="chart-criteria">
      <summary title="Критерии для этого графика">
        Критерии · {selected.length}
      </summary>
      <div className="chart-criteria-menu">
        <div className="chart-criteria-head">
          <span>Критерии графика</span>
          {selected.length > 0 && (
            <button className="btn b-dim" onClick={() => store.clearChartCriteria(symbol, timeframe)}>
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
                onChange={() => store.toggleChartCriterion(symbol, timeframe, criterion.id)}
              />
              <span>{criterion.label}</span>
            </label>
          ))}
        </div>
      </div>
    </details>
  );
}
