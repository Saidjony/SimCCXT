import { store, useSim } from "../state/store";

/** Drawings list (select / delete). Tool buttons live in the chart (DrawBar). */
export function DrawTools(): JSX.Element {
  const drawings = useSim((s) => s.drawings);
  const selectedId = useSim((s) => s.selectedId);
  const positionSizeUsdt = useSim((s) => s.positionSizeUsdt);

  return (
    <div className="card">
      <h4>Рисунки</h4>
      <div className="row">
        <span className="lbl">Позиция, USDT</span>
        <input
          value={String(positionSizeUsdt)}
          onChange={(e) => {
            const v = Number(e.target.value);
            if (Number.isFinite(v) && v > 0) store.setPositionSizeUsdt(v);
          }}
          style={{ width: 90 }}
        />
      </div>
      <div className="lbl" style={{ marginTop: 6 }}>
        drag — двигать · правый клик — удалить · Del — удалить выбранное · Esc — снять
      </div>
      {drawings.length === 0 && <div className="lbl">иконки инструментов — в правом верхнем углу графика</div>}
      {drawings.map((d) => (
        <div
          className={"rowline" + (d.id === selectedId ? " sel" : "")}
          key={d.id}
          onClick={() => store.selectDrawing(d.id)}
          style={{ cursor: "pointer" }}
        >
          <span>{d.title}</span>
          <button
            className="btn b-red"
            onClick={(e) => {
              e.stopPropagation();
              store.removeDrawing(d.id);
            }}
          >
            ✕
          </button>
        </div>
      ))}
    </div>
  );
}
