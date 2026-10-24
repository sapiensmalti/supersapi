// Drives the real extension against a real sapid (with tests/fake-docker.sh as docker).
// Usage: node test/extension-harness.mjs <sapid-bin>
import { spawn as spawnProc } from "node:child_process";
import { mkdtempSync, readFileSync, existsSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const here = fileURLToPath(new URL(".", import.meta.url));
const sapid = process.argv[2];
const fake = join(here, "..", "sapid", "tests", "fake-docker.sh");
const state = mkdtempSync(join(tmpdir(), "ss-state-"));
const fakeState = mkdtempSync(join(tmpdir(), "ss-fake-"));
const root = mkdtempSync(join(tmpdir(), "ss-root-"));
mkdirSync(join(root, "sub"));

const broker = spawnProc(sapid, ["serve", "--state-dir", state, "--root-dir", root, "--image", "x", "--docker", fake, "--max-children", "3"],
  { env: { ...process.env, FAKE_DOCKER_STATE: fakeState, SAPID_BIN: sapid }, stdio: ["ignore", "ignore", "inherit"] });
process.on("exit", () => broker.kill("SIGKILL"));
for (let i = 0; i < 100 && !existsSync(join(state, "token")); i++) await new Promise(r => setTimeout(r, 50));
process.env.SAPID_ADDR = `unix://${join(state, "sapid.sock")}`;
process.env.SAPID_TOKEN = readFileSync(join(state, "token"), "utf8").trim();

const tools = new Map(); const commands = new Map();
const api = { registerTool: (t) => tools.set(t.name, t), registerCommand: (n, c) => commands.set(n, c) };
const mod = await import(join(here, "..", "extension", "index.ts"));
mod.default(api);

let failures = 0;
const check = (name, cond, extra = "") => { console.log(`  ${cond ? "ok  " : "FAIL"} ${name}${extra ? `: ${extra}` : ""}`); if (!cond) failures++; };

check("registers swarm, swarm_ps, swarm_stop", ["swarm", "swarm_ps", "swarm_stop"].every(n => tools.has(n)));
check("registers /swarm", commands.has("swarm"));

const updates = [];
const swarm = tools.get("swarm");
let r = await swarm.execute("c1", { tasks: [
  { task: "echo:alpha", name: "a" },
  { task: "echo:beta", dir: "sub", access: "read_only" },
  { task: "fail", name: "c" },
  { task: "echo:refused", dir: "..", name: "d" },
] }, undefined, (u) => updates.push(u));
const text = r.content[0].text;
check("parallel outputs returned", text.includes("alpha") && text.includes("beta"), text.slice(0, 200));
check("failure reported with exit code", /\[3\] c .*exited \(exit 3\)/.test(text), text.match(/\[3\][^\n]*/)?.[0]);
check("policy refusal surfaced", /\[4\] d .*refused/.test(text) && text.includes("policy_violation"), text.match(/\[4\][^\n]*/)?.[0]);
check("progress updates streamed", updates.length >= 4, String(updates.length));
check("usage aggregated", r.details.outcomes[0].usage?.turns === 1 && r.details.outcomes[0].model === "fake-model");
check("not all failed → isError false", r.isError === false);
check("renderResult works", typeof swarm.renderResult(r, { expanded: true }, { fg: (_c, s) => s, bold: (s) => s }) === "string");
check("renderCall works", typeof swarm.renderCall({ tasks: [{ task: "x" }] }, { fg: (_c, s) => s, bold: (s) => s }) === "string");

r = await swarm.execute("c2", { tasks: [{ task: "fail" }] }, undefined, undefined);
check("all failed → isError true", r.isError === true);

const ps = await tools.get("swarm_ps").execute("c3", {}, undefined);
check("swarm_ps lists nodes", ps.details.nodes.length === 4, String(ps.details.nodes.length));

// abort: start a slow child, abort, expect it stopped
const ac = new AbortController();
const p = swarm.execute("c4", { tasks: [{ task: "sleep:30" }] }, ac.signal, undefined);
await new Promise(r => setTimeout(r, 400)); ac.abort();
let aborted = false; try { await p; } catch (e) { aborted = /abort/.test(String(e)); }
check("abort rejects", aborted);
await new Promise(r => setTimeout(r, 300));
const after = await tools.get("swarm_ps").execute("c5", {}, undefined);
const slow = after.details.nodes.find(n => n.task.startsWith("sleep"));
check("aborted child is stopped", slow?.state === "stopped", slow?.state);

const st = await tools.get("swarm_stop").execute("c6", { id: "999" }, undefined).catch(e => e);
check("swarm_stop on unknown id errors", st instanceof Error && st.code === "not_found", `${st.code}: ${st.message}`);

broker.kill("SIGTERM");
console.log(failures ? `\n${failures} FAILED` : "\nall extension checks passed");
process.exit(failures ? 1 : 0);
