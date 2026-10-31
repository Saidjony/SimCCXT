// Compile the browser engine and put it where the page fetches it from.
//
// The artifact is gitignored (it is a build output), so a fresh clone has no
// `frontend/public/simccxt_wasm.wasm`. That is why `npm run dev` and
// `npm run build` must both run this script: without it the page asks for the
// file, the static server answers with index.html, and the browser fails with
// the opaque `WebAssembly.instantiate(): expected magic word 00 61 73 6d`.
import { execFileSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const frontendDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repositoryDir = resolve(frontendDir, "..");
const wasmTarget = resolve(
  repositoryDir,
  "target/wasm32-unknown-unknown/release/simccxt_wasm.wasm",
);
const publicDir = resolve(frontendDir, "public");
const wasmPublic = resolve(publicDir, "simccxt_wasm.wasm");
const force = process.argv.includes("--force");

const WASM_MAGIC = [0x00, 0x61, 0x73, 0x6d]; // "\0asm"

/** Newest mtime of the Rust sources this artifact is built from. */
function newestSourceTime(dir) {
  let newest = 0;
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (entry.name === "target" || entry.name === "node_modules" || entry.name === ".git") {
      continue;
    }
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      newest = Math.max(newest, newestSourceTime(full));
    } else if (entry.name.endsWith(".rs") || entry.name === "Cargo.toml") {
      newest = Math.max(newest, statSync(full).mtimeMs);
    }
  }
  return newest;
}

function mtime(path) {
  try {
    return statSync(path).mtimeMs;
  } catch {
    return 0;
  }
}

const sourceTime = Math.max(
  newestSourceTime(join(repositoryDir, "src")),
  mtime(join(repositoryDir, "Cargo.toml")),
  mtime(join(repositoryDir, "Cargo.lock")),
);
const upToDate =
  !force && existsSync(wasmTarget) && mtime(wasmTarget) >= sourceTime;

if (upToDate) {
  mkdirSync(publicDir, { recursive: true });
  if (mtime(wasmPublic) < mtime(wasmTarget)) copyFileSync(wasmTarget, wasmPublic);
  console.log("[wasm] up to date");
  process.exit(0);
}

try {
  execFileSync(
    "cargo",
    ["build", "-p", "simccxt-wasm", "--release", "--target", "wasm32-unknown-unknown"],
    { cwd: repositoryDir, stdio: "inherit" },
  );
} catch (error) {
  console.error(
    [
      "",
      "[wasm] не удалось собрать simccxt_wasm.wasm.",
      "Проверьте, что установлены Rust и wasm-цель:",
      "",
      "    rustup target add wasm32-unknown-unknown",
      "    cargo build -p simccxt-wasm --release --target wasm32-unknown-unknown",
      "",
      "Затем повторите: npm run build:wasm",
      "",
    ].join("\n"),
  );
  process.exit(1);
}

const bytes = readFileSync(wasmTarget);
if (bytes.length < 8 || !WASM_MAGIC.every((b, i) => bytes[i] === b)) {
  console.error(`[wasm] ${wasmTarget} is not a WebAssembly module`);
  process.exit(1);
}
mkdirSync(publicDir, { recursive: true });
copyFileSync(wasmTarget, wasmPublic);
console.log(`[wasm] ${(bytes.length / 1024).toFixed(0)} KiB -> frontend/public/simccxt_wasm.wasm`);
