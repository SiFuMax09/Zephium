// Runs the Tauri CLI. Work (the agent product) has no Linux adapters yet, so
// on Linux Cargo builds without default features.
import { spawnSync } from "node:child_process";

const args = process.argv.slice(2);
if (process.platform === "linux") {
  args.push("--", "--no-default-features");
}
const run = spawnSync("pnpm", ["exec", "tauri", ...args], {
  stdio: "inherit",
  shell: process.platform === "win32",
});
process.exit(run.status ?? 1);
