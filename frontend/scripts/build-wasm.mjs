import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const frontendDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repositoryDir = resolve(frontendDir, "..");
const wasmTarget = resolve(repositoryDir, "target/wasm32-unknown-unknown/release/simccxt_wasm.wasm");
const publicDir = resolve(frontendDir, "public");

execFileSync("cargo", ["build", "-p", "simccxt-wasm", "--release", "--target", "wasm32-unknown-unknown"], {
  cwd: repositoryDir,
  stdio: "inherit",
});
mkdirSync(publicDir, { recursive: true });
copyFileSync(wasmTarget, resolve(publicDir, "simccxt_wasm.wasm"));
