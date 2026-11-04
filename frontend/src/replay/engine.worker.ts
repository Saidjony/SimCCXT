interface InitMessage {
  type: "init";
  symbol: string;
  timeframe: string;
  start_ms: number;
  step_ms: number;
  market_kind: "spot" | "linear";
  engine_id: string;
  candles: number[][];
  exchange: string;
  speed: number;
  reveal_reset_ms: number;
  range_end_ms: number;
  oi: Array<[number, number]>;
  oi_period: string;
}

type EngineCommand =
  | { type: "play" }
  | { type: "pause" }
  | { type: "step"; steps?: number }
  | { type: "reset"; timestamp_ms: number }
  | { type: "set_step"; step_ms: number }
  | { type: "set_speed"; speed: number }
  | {
      type: "switch_series";
      symbol: string;
      exchange: string;
      timeframe: string;
      step_ms: number;
      speed: number;
      market_kind: "spot" | "linear";
      engine_id: string;
      reveal_reset_ms: number;
      range_end_ms: number;
      oi: Array<[number, number]>;
      oi_period: string;
      preserve_clock: boolean;
      candles: number[][];
    }
  | {
      type: "place_order";
      symbol?: string;
      side: string;
      order_type: string;
      quantity: number;
      price?: number;
      stop_price?: number;
      stop_loss_price?: number;
      tp_price?: number;
      risk_usdt?: number;
      time_in_force?: string;
      reduce_only?: boolean;
      post_only?: boolean;
    }
  | { type: "close_position"; symbol?: string; quantity?: number }
  | { type: "cancel_order"; symbol?: string; order_id: string };

interface CommandMessage {
  type: "command";
  command: EngineCommand;
}

interface SnapshotMessage {
  type: "snapshot";
  include_fills: true;
}

type WorkerMessage = InitMessage | CommandMessage | SnapshotMessage;

interface WasmExports extends WebAssembly.Exports {
  memory: WebAssembly.Memory;
  alloc: (size: number) => number;
  dealloc: (pointer: number, length: number) => void;
  handle_message: (pointer: number, length: number) => bigint;
  release_result: (pointer: number, length: number) => void;
}

const encoder = new TextEncoder();
const decoder = new TextDecoder();
let wasm: WasmExports | undefined;
let timer: ReturnType<typeof setTimeout> | undefined;
let speed = 1;

function wasmRequest(request: Record<string, unknown>): Record<string, unknown> {
  if (!wasm) throw new Error("WebAssembly engine has not been initialized");
  const bytes = encoder.encode(JSON.stringify(request));
  const pointer = wasm.alloc(bytes.length);
  try {
    new Uint8Array(wasm.memory.buffer, pointer, bytes.length).set(bytes);
    const packed = wasm.handle_message(pointer, bytes.length);
    const responsePointer = Number(packed >> 32n);
    const responseLength = Number(packed & 0xffff_ffffn);
    try {
      const responseBytes = new Uint8Array(wasm.memory.buffer, responsePointer, responseLength).slice();
      const response = JSON.parse(decoder.decode(responseBytes)) as {
        ok: boolean;
        error?: string;
        snapshot?: Record<string, unknown>;
      };
      if (!response.ok) throw new Error(response.error ?? "Unknown WebAssembly error");
      return response;
    } finally {
      wasm.release_result(responsePointer, responseLength);
    }
  } finally {
    wasm.dealloc(pointer, bytes.length);
  }
}

function publish(request: Record<string, unknown>): Record<string, unknown> {
  const result = wasmRequest(request);
  if (result.snapshot) self.postMessage({ type: "snapshot", snapshot: result.snapshot });
  return (result.snapshot as Record<string, unknown> | undefined) ?? {};
}

function stopTimer(): void {
  if (timer !== undefined) clearTimeout(timer);
  timer = undefined;
}

function scheduleTick(): void {
  stopTimer();
  timer = setTimeout(() => {
    try {
      const state = publish({ type: "tick", steps: 1 })["replay"] as
        | { paused?: boolean; done?: boolean }
        | undefined;
      if (state?.paused || state?.done) {
        stopTimer();
        if (state?.done) publish({ type: "pause" });
      } else {
        scheduleTick();
      }
    } catch (error) {
      stopTimer();
      self.postMessage({ type: "error", message: String(error) });
    }
  }, Math.max(10, Math.round(1000 / speed)));
}

self.onmessage = async (event: MessageEvent<WorkerMessage>) => {
  try {
    const message = event.data;
    if (message.type === "init") {
      stopTimer();
      speed = message.speed;
      const { type: _type, ...input } = message;
      const response = await fetch(new URL("/simccxt_wasm.wasm", self.location.origin), {
        cache: "no-cache",
      });
      if (!response.ok) {
        throw new Error(
          `Не удалось загрузить simccxt_wasm.wasm: HTTP ${response.status}. ` +
            `Соберите движок: cd frontend && npm run build:wasm`,
        );
      }
      const bytes = new Uint8Array(await response.arrayBuffer());
      // A missing artifact is usually answered with index.html by the static
      // server, and WebAssembly then fails with "expected magic word ... found
      // 3c 21 64 6f 63" (that is "<!doc"), which says nothing about the cause.
      const isWasm =
        bytes.length >= 4 &&
        bytes[0] === 0x00 &&
        bytes[1] === 0x61 &&
        bytes[2] === 0x73 &&
        bytes[3] === 0x6d;
      if (!isWasm) {
        const head = new TextDecoder().decode(bytes.slice(0, 40)).trim().slice(0, 30);
        throw new Error(
          `simccxt_wasm.wasm не собран: вместо WebAssembly пришёл ответ «${head}…». ` +
            `Выполните: cd frontend && npm run build:wasm (и перезагрузите страницу).`,
        );
      }
      const { instance } = await WebAssembly.instantiate(bytes, {});
      wasm = instance.exports as WasmExports;
      publish({ type: "init", ...input });
      return;
    }
    if (message.type === "snapshot") {
      // Forward `include_fills`: the wasm engine only re-sends the whole fill
      // history when asked for it, and a reconnecting session needs exactly
      // that to rebuild the diary after a server restart.
      publish({ type: "snapshot", include_fills: message.include_fills === true });
      return;
    }
    const { command } = message;
    if (command.type === "set_speed") speed = command.speed;
    if (command.type === "switch_series") {
      stopTimer();
      speed = command.speed;
    }
    const snapshot = publish({ ...command });
    if (command.type === "pause" || command.type === "reset") stopTimer();
    if (command.type === "play") scheduleTick();
    if (command.type === "switch_series") {
      const state = snapshot["replay"] as
        | { paused?: boolean }
        | undefined;
      if (state?.paused === false) scheduleTick();
    }
  } catch (error) {
    self.postMessage({ type: "error", message: String(error) });
  }
};
