export interface SharedCrosshair {
  timestamp: number;
  price?: number;
}

interface CrosshairEvent {
  position: SharedCrosshair | null;
  source: string;
}

type Listener = (event: CrosshairEvent) => void;

const listeners = new Set<Listener>();
let current: CrosshairEvent = { position: null, source: "" };
let frame = 0;

export function publishCrosshair(position: SharedCrosshair | null, source: string): void {
  current = { position, source };
  if (frame) return;
  frame = window.requestAnimationFrame(() => {
    frame = 0;
    for (const listener of listeners) listener(current);
  });
}

export function subscribeCrosshair(listener: Listener): () => void {
  listeners.add(listener);
  listener(current);
  return () => {
    listeners.delete(listener);
  };
}
