/**
 * Minimal JSONL client for the sapid broker.
 *
 * Transport is `unix://PATH` or `tcp://HOST:PORT` from `SAPID_ADDR`; the token
 * comes from `SAPID_TOKEN`. One connection per call keeps the code trivially
 * correct under cancellation: aborting closes the socket, nothing else.
 */

import * as net from "node:net";

export type Access = "none" | "read_only" | "read_write";
export type Network = "none" | "full";

export interface SpawnParams {
	task: string;
	dir?: string;
	access?: Access;
	network?: Network;
	model?: string;
	timeout_secs?: number;
	cpus?: number;
	memory_mib?: number;
	name?: string;
	approve_project?: boolean;
}

export interface Usage {
	turns: number;
	input: number;
	output: number;
	cache_read: number;
	cache_write: number;
	cost: number;
}

export interface NodeInfo {
	id: string;
	parent?: string;
	depth: number;
	name: string;
	task: string;
	dir: string;
	access: Access;
	network: Network;
	timeout_secs: number;
	state: "running" | "exited" | "stopped" | "failed";
	exit_code?: number;
	reason?: string;
	started_at: number;
	finished_at?: number;
	usage: Usage;
	model?: string;
}

export interface WaitResult {
	node: NodeInfo;
	output: string;
	timed_out: boolean;
}

export interface HelloResult {
	protocol: number;
	server: string;
	node: NodeInfo;
	limits: { max_children: number; max_depth: number; max_total: number };
}

export class BrokerError extends Error {
	readonly code: string;
	constructor(code: string, message: string) {
		super(message);
		this.name = "BrokerError";
		this.code = code;
	}
}

export interface Endpoint {
	addr: string;
	token: string;
}

/** Read the broker endpoint from the environment, or `undefined` if not in a swarm. */
export function endpointFromEnv(env: NodeJS.ProcessEnv = process.env): Endpoint | undefined {
	const addr = env.SAPID_ADDR;
	const token = env.SAPID_TOKEN;
	if (!addr || !token) return undefined;
	return { addr, token };
}

function connect(addr: string): net.Socket {
	if (addr.startsWith("unix://")) return net.connect(addr.slice("unix://".length));
	if (addr.startsWith("tcp://")) {
		const hostPort = addr.slice("tcp://".length);
		const i = hostPort.lastIndexOf(":");
		if (i < 0) throw new BrokerError("bad_request", `SAPID_ADDR has no port: ${addr}`);
		return net.connect(Number(hostPort.slice(i + 1)), hostPort.slice(0, i));
	}
	throw new BrokerError("bad_request", `SAPID_ADDR must start with unix:// or tcp://: ${addr}`);
}

/**
 * Perform one operation. `onEvent` receives streaming events. Resolves with
 * the result, rejects with `BrokerError` on a protocol-level error.
 */
export function call<T>(
	ep: Endpoint,
	op: string,
	params: unknown,
	options: { signal?: AbortSignal; onEvent?: (event: unknown) => void } = {},
): Promise<T> {
	return new Promise<T>((resolve, reject) => {
		const id = "1";
		let settled = false;
		const sock = connect(ep.addr);
		let buf = "";

		const finish = (fn: () => void) => {
			if (settled) return;
			settled = true;
			options.signal?.removeEventListener("abort", onAbort);
			sock.destroy();
			fn();
		};
		const onAbort = () => finish(() => reject(new BrokerError("aborted", "aborted")));
		if (options.signal?.aborted) return onAbort();
		options.signal?.addEventListener("abort", onAbort, { once: true });

		sock.setEncoding("utf8");
		sock.on("connect", () => {
			const req = params === undefined ? { id, token: ep.token, op } : { id, token: ep.token, op, params };
			sock.write(`${JSON.stringify(req)}\n`);
		});
		sock.on("data", (chunk: string) => {
			buf += chunk;
			let nl: number;
			while ((nl = buf.indexOf("\n")) >= 0) {
				const line = buf.slice(0, nl);
				buf = buf.slice(nl + 1);
				if (!line.trim()) continue;
				let frame: any;
				try {
					frame = JSON.parse(line);
				} catch {
					continue;
				}
				if (frame.id !== id) continue;
				if ("event" in frame) {
					options.onEvent?.(frame.event);
				} else if (frame.ok === true) {
					finish(() => resolve(frame.result as T));
				} else if (frame.ok === false) {
					finish(() => reject(new BrokerError(frame.error?.code ?? "error", frame.error?.message ?? "unknown error")));
				}
			}
		});
		sock.on("error", (e) => finish(() => reject(new BrokerError("connection", `broker: ${e.message}`))));
		sock.on("close", () => finish(() => reject(new BrokerError("connection", "broker closed the connection"))));
	});
}

export const hello = (ep: Endpoint, signal?: AbortSignal) => call<HelloResult>(ep, "hello", undefined, { signal });
export const spawn = (ep: Endpoint, p: SpawnParams, signal?: AbortSignal) =>
	call<{ id: string; container: string }>(ep, "spawn", p, { signal });
export const list = (ep: Endpoint, signal?: AbortSignal) => call<{ nodes: NodeInfo[] }>(ep, "list", undefined, { signal });
export const status = (ep: Endpoint, id: string, signal?: AbortSignal) => call<NodeInfo>(ep, "status", { id }, { signal });
export const stop = (ep: Endpoint, id: string, signal?: AbortSignal) =>
	call<{ stopped: string[] }>(ep, "stop", { id }, { signal });
export const wait = (
	ep: Endpoint,
	id: string,
	opts: { stream?: boolean; timeout_secs?: number; signal?: AbortSignal; onEvent?: (e: unknown) => void } = {},
) => call<WaitResult>(ep, "wait", { id, stream: opts.stream ?? false, timeout_secs: opts.timeout_secs }, opts);
export const logs = (ep: Endpoint, id: string, onLine: (line: string) => void, signal?: AbortSignal) =>
	call<{ lines: number }>(ep, "logs", { id, follow: false }, {
		signal,
		onEvent: (e: any) => {
			if (typeof e?.line === "string") onLine(e.line);
		},
	});
