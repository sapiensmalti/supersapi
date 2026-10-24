/**
 * supersapi swarm extension for pi.
 *
 * Gives the agent three tools and one command:
 *
 *   swarm        spawn one or more children and wait for them (parallel)
 *   swarm_ps     list descendants and their state
 *   swarm_stop   stop a descendant and its subtree
 *   /swarm       show the tree in the TUI
 *
 * Children are real containers created by the sapid broker on the host. The
 * extension never touches Docker; it speaks the broker protocol over the
 * socket mounted at SAPID_ADDR with the token in SAPID_TOKEN. A child can only
 * ever get a subset of this node's own access, so the model may call `swarm`
 * freely: the broker refuses anything wider.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import {
	type Access,
	BrokerError,
	type Endpoint,
	endpointFromEnv,
	hello,
	list,
	type NodeInfo,
	spawn,
	stop,
	wait,
	type WaitResult,
} from "./client.ts";

const MAX_TASKS = 16;
const OUTPUT_CAP = 48 * 1024;

const TaskSchema = Type.Object({
	task: Type.String({ description: "Complete, self-contained instructions for the child. It starts with no memory of this conversation." }),
	dir: Type.Optional(
		Type.String({
			description: "Subdirectory of the current workspace the child works in (default: the whole workspace). It cannot see anything above it.",
		}),
	),
	access: Type.Optional(
		Type.Union([Type.Literal("read_write"), Type.Literal("read_only"), Type.Literal("none")], {
			description: "Filesystem access: read_write (default, inherit), read_only for scouts/reviewers, none for pure compute on a scratch volume.",
		}),
	),
	network: Type.Optional(
		Type.Union([Type.Literal("full"), Type.Literal("none")], {
			description: "Network reach. none = loopback only (the child cannot reach model providers either; use only with local models).",
		}),
	),
	model: Type.Optional(Type.String({ description: "Model id for the child, e.g. a cheaper model for a scout." })),
	timeout_secs: Type.Optional(Type.Number({ description: "Wall-clock budget in seconds. Must not exceed this node's own." })),
	name: Type.Optional(Type.String({ description: "Short label for listings." })),
});

const SwarmParams = Type.Object({
	tasks: Type.Array(TaskSchema, {
		description: `One entry per child; all run in parallel. Max ${MAX_TASKS}.`,
		minItems: 1,
		maxItems: MAX_TASKS,
	}),
});

const PsParams = Type.Object({});
const StopParams = Type.Object({ id: Type.String({ description: "Node id from swarm_ps or a previous swarm call." }) });

interface TaskOutcome {
	index: number;
	id?: string;
	name: string;
	task: string;
	dir?: string;
	access?: Access;
	state: "spawning" | "running" | "exited" | "stopped" | "failed" | "refused";
	exit_code?: number;
	reason?: string;
	output: string;
	error?: string;
	usage?: NodeInfo["usage"];
	model?: string;
	recent: string[];
}

interface SwarmDetails {
	outcomes: TaskOutcome[];
}

function cap(s: string): string {
	if (Buffer.byteLength(s, "utf8") <= OUTPUT_CAP) return s;
	const cut = Buffer.from(s, "utf8").subarray(0, OUTPUT_CAP).toString("utf8");
	return `${cut}\n\n[truncated; full output in swarm logs]`;
}

function fmtUsage(u?: NodeInfo["usage"], model?: string): string {
	if (!u) return "";
	const parts: string[] = [];
	if (u.turns) parts.push(`${u.turns} turn${u.turns === 1 ? "" : "s"}`);
	if (u.input) parts.push(`↑${fmtTokens(u.input)}`);
	if (u.output) parts.push(`↓${fmtTokens(u.output)}`);
	if (u.cost) parts.push(`$${u.cost.toFixed(4)}`);
	if (model) parts.push(model);
	return parts.join(" ");
}

function fmtTokens(n: number): string {
	if (n < 1000) return String(n);
	if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`;
	return `${(n / 1_000_000).toFixed(1)}M`;
}

function icon(state: TaskOutcome["state"]): string {
	switch (state) {
		case "exited":
			return "✓";
		case "running":
		case "spawning":
			return "⏳";
		default:
			return "✗";
	}
}

function summarize(outcomes: TaskOutcome[]): string {
	return outcomes
		.map((o) => {
			const head = `## [${o.index + 1}] ${o.name}${o.id ? ` (node ${o.id})` : ""} — ${o.state}${
				o.exit_code !== undefined && o.exit_code !== 0 ? ` (exit ${o.exit_code})` : ""
			}${o.reason ? ` — ${o.reason}` : ""}`;
			const body = o.error ? `Error: ${o.error}` : o.output || "(no output)";
			return `${head}\n\n${cap(body)}`;
		})
		.join("\n\n");
}

export default function (pi: ExtensionAPI) {
	const ep: Endpoint | undefined = endpointFromEnv();

	if (!ep) {
		// Not inside a swarm: register nothing the model could call, but tell
		// a human what happened if they ask.
		pi.registerCommand("swarm", {
			description: "Show swarm status (requires supersapi)",
			handler: async (_args, ctx) => {
				ctx.ui.notify("Not running inside a supersapi swarm (SAPID_ADDR/SAPID_TOKEN unset). Start with `supersapi`.", "warning");
			},
		});
		return;
	}

	pi.registerTool({
		name: "swarm",
		label: "Swarm",
		description: [
			"Delegate work to parallel child agents, each in its own isolated container with its own context window.",
			"Use it to fan out independent subtasks: explore several directories, implement several files, run reviews.",
			"Each child only sees the directory you give it (or this workspace), and never more than you can.",
			"Write tasks as complete briefs: goal, constraints, what to report back. Children cannot ask you questions.",
			"Returns each child's final message. Children write to the workspace directly; coordinate directories to avoid conflicts.",
		].join(" "),
		parameters: SwarmParams,
		async execute(_toolCallId, params, signal, onUpdate) {
			const outcomes: TaskOutcome[] = params.tasks.map((t, index) => ({
				index,
				name: t.name ?? `task ${index + 1}`,
				task: t.task,
				dir: t.dir,
				access: t.access,
				state: "spawning",
				output: "",
				recent: [],
			}));
			const details = (): SwarmDetails => ({ outcomes: outcomes.map((o) => ({ ...o })) });
			const progress = () => {
				const done = outcomes.filter((o) => o.state !== "running" && o.state !== "spawning").length;
				onUpdate?.({
					content: [{ type: "text", text: `${done}/${outcomes.length} done` }],
					details: details(),
				});
			};

			await Promise.all(
				params.tasks.map(async (t, i) => {
					const o = outcomes[i];
					try {
						const { id } = await spawn(
							ep,
							{
								task: t.task,
								dir: t.dir,
								access: t.access,
								network: t.network,
								model: t.model,
								timeout_secs: t.timeout_secs,
								name: t.name,
							},
							signal,
						);
						o.id = id;
						o.state = "running";
						progress();
						const r: WaitResult = await wait(ep, id, {
							stream: true,
							signal,
							onEvent: (e: any) => {
								if (e?.type === "assistant" && typeof e.text === "string" && e.text.trim()) {
									o.recent.push(e.text.split("\n")[0].slice(0, 120));
								} else if (e?.type === "tool_execution_start" && e.tool) {
									const arg = e.args?.command ?? e.args?.path ?? "";
									o.recent.push(`$ ${e.tool} ${String(arg).slice(0, 100)}`);
								}
								if (o.recent.length > 8) o.recent.shift();
								progress();
							},
						});
						o.state = r.node.state;
						o.exit_code = r.node.exit_code;
						o.reason = r.node.reason;
						o.output = r.output;
						o.usage = r.node.usage;
						o.model = r.node.model;
					} catch (e) {
						if (e instanceof BrokerError) {
							o.state = e.code === "policy_violation" || e.code === "limit_exceeded" ? "refused" : "failed";
							o.error = `${e.code}: ${e.message}`;
						} else {
							o.state = "failed";
							o.error = e instanceof Error ? e.message : String(e);
						}
					}
					progress();
				}),
			);

			if (signal?.aborted) {
				// Stop every child we started; the broker ignores ones already finished.
				// Use a fresh (non-aborted) signal: the stop must go through.
				await Promise.all(
					outcomes.filter((o) => o.id).map((o) => stop(ep, o.id as string).catch(() => undefined)),
				);
				throw new Error("swarm aborted");
			}

			const failed = outcomes.filter((o) => o.state !== "exited" || (o.exit_code ?? 0) !== 0).length;
			return {
				content: [{ type: "text", text: summarize(outcomes) }],
				details: details(),
				isError: failed === outcomes.length && outcomes.length > 0,
			};
		},
		renderCall(args, theme) {
			const n = args.tasks?.length ?? 0;
			const names = (args.tasks ?? []).map((t: any) => t.name ?? t.task.split("\n")[0].slice(0, 40)).join(", ");
			return theme.fg("toolTitle", theme.bold("swarm ")) + theme.fg("muted", `${n} child${n === 1 ? "" : "ren"}: ${names}`);
		},
		renderResult(result, { expanded }, theme) {
			const d = result.details as SwarmDetails | undefined;
			if (!d) return theme.fg("muted", "(no details)");
			const lines: string[] = [];
			for (const o of d.outcomes) {
				const color = o.state === "exited" && (o.exit_code ?? 0) === 0 ? "success" : o.state === "running" || o.state === "spawning" ? "warning" : "error";
				lines.push(
					`${theme.fg(color, icon(o.state))} ${theme.bold(o.name)}${o.id ? theme.fg("muted", ` #${o.id}`) : ""}${o.dir ? theme.fg("muted", ` ${o.dir}`) : ""}${o.access && o.access !== "read_write" ? theme.fg("muted", ` [${o.access}]`) : ""}  ${theme.fg("dim", fmtUsage(o.usage, o.model))}`,
				);
				if (o.error) lines.push(`    ${theme.fg("error", o.error)}`);
				const tail = expanded ? o.recent : o.recent.slice(-2);
				for (const r of tail) lines.push(`    ${theme.fg("muted", r)}`);
				if (expanded && o.output) {
					lines.push("");
					for (const l of o.output.split("\n").slice(0, 60)) lines.push(`    ${l}`);
				}
			}
			return lines.join("\n");
		},
	});

	pi.registerTool({
		name: "swarm_ps",
		label: "Swarm status",
		description: "List this node's child agents (and their children) with state, directory, access and usage.",
		parameters: PsParams,
		async execute(_id, _params, signal) {
			const { nodes } = await list(ep, signal);
			const text = nodes.length
				? nodes
						.map(
							(n) =>
								`${n.id} depth=${n.depth} ${n.state}${n.exit_code !== undefined ? `(${n.exit_code})` : ""} dir=${n.dir} access=${n.access} ${fmtUsage(n.usage, n.model)}  ${n.task}`,
						)
						.join("\n")
				: "(no children)";
			return { content: [{ type: "text", text }], details: { nodes } };
		},
	});

	pi.registerTool({
		name: "swarm_stop",
		label: "Swarm stop",
		description: "Stop a child agent and everything it spawned.",
		parameters: StopParams,
		async execute(_id, params, signal) {
			const { stopped } = await stop(ep, params.id, signal);
			return { content: [{ type: "text", text: stopped.length ? `stopped ${stopped.join(", ")}` : "nothing was running" }], details: { stopped } };
		},
	});

	pi.registerCommand("swarm", {
		description: "Show the swarm tree",
		handler: async (_args, ctx) => {
			try {
				const me = await hello(ep);
				const { nodes } = await list(ep);
				const head = `node ${me.node.id} depth ${me.node.depth} · access ${me.node.access} · limits ${me.limits.max_children}/${me.limits.max_depth}/${me.limits.max_total} (children/depth/total)`;
				const body = nodes.length
					? nodes.map((n) => `${"  ".repeat(Math.max(0, n.depth - me.node.depth - 1))}${n.id} ${n.state} ${n.dir} [${n.access}] ${n.task}`).join("\n")
					: "(no children)";
				ctx.ui.notify(`${head}\n${body}`, "info");
			} catch (e) {
				ctx.ui.notify(`swarm: ${e instanceof Error ? e.message : String(e)}`, "error");
			}
		},
	});
}
