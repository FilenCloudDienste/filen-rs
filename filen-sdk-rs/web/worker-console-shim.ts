// Forwards a worker's console calls and uncaught errors to the test iframe, the only console vitest's
// browser mode captures (vitest-dev/vitest#7439), while the SDK and its Rust logging run on workers.
// worker-console-relay.setup.ts replays each message there. Every worker script the tests load
// calls forwardWorkerConsole: the SDK's and rayon's through workerConsoleShim in vitest.config.ts,
// the service worker in sw.ts. A call, not a side-effect import, because package.json's
// `sideEffects` lets the service worker's bundler drop the latter. Calling it from a module body
// is early enough: the wasm-bindgen glue looks the console up on every call, and init() runs later.

export const WORKER_CONSOLE_CHANNEL = "filen-worker-console"

export type WorkerConsoleLevel = "log" | "info" | "debug" | "warn" | "error"

export type WorkerConsoleMessage = {
	level: WorkerConsoleLevel
	where: string
	// Stringified, as a message must be cloneable. The Rust side only ever logs strings.
	args: string[]
}

// A no-op outside a worker, since the iframe also loads workerHelpers.js, and on a second call.
export function forwardWorkerConsole() {
	const scope = globalThis as typeof globalThis & { __filenWorkerConsole?: boolean }

	if (typeof WorkerGlobalScope === "undefined" || !(scope instanceof WorkerGlobalScope) || scope.__filenWorkerConsole) {
		return
	}

	scope.__filenWorkerConsole = true

	const channel = new BroadcastChannel(WORKER_CONSOLE_CHANNEL)
	const where = scope.location.pathname.split("/").pop() || "worker"

	const toText = (value: unknown): string => {
		if (typeof value === "string") {
			return value
		}

		if (value instanceof Error) {
			return value.stack?.includes(value.message) ? value.stack : `${value.name}: ${value.message}\n${value.stack ?? ""}`
		}

		if (typeof value === "object" && value !== null) {
			try {
				return JSON.stringify(value) ?? String(value)
			} catch {
				return String(value)
			}
		}

		return String(value)
	}

	const post = (level: WorkerConsoleLevel, args: unknown[]) => {
		try {
			channel.postMessage({ level, where, args: args.map(toText) } satisfies WorkerConsoleMessage)
		} catch {
			// the worker's own console still has the line
		}
	}

	for (const level of ["log", "info", "debug", "warn", "error"] as const) {
		const original = console[level]

		console[level] = (...args: unknown[]) => {
			post(level, args)
			original.apply(console, args)
		}
	}

	// A wasm trap or an uncaught throw never reaches the console. Listening leaves the default
	// handling, the worker's console and its Worker object's error event, as it was.
	scope.addEventListener("error", (event: ErrorEvent) => {
		post("error", [`uncaught: ${event.message} (${event.filename}:${event.lineno}:${event.colno})`, ...(event.error ? [event.error] : [])])
	})

	scope.addEventListener("unhandledrejection", (event: PromiseRejectionEvent) => {
		post("error", ["unhandled rejection:", event.reason])
	})
}
