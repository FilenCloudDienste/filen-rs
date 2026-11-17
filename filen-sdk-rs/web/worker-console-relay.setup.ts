import { afterAll } from "vitest"
import { WORKER_CONSOLE_CHANNEL, type WorkerConsoleMessage } from "./worker-console-shim.ts"

// Replays in the test iframe what the workers log (worker-console-shim.ts), through the console
// vitest captures, so a line prints under the test running when it arrives. The prefix goes into
// the format string and so must not add a directive of its own; the %c directives the Rust
// logging layer emits are consumed by vitest's formatting along with their CSS arguments.
const channel = new BroadcastChannel(WORKER_CONSOLE_CHANNEL)

channel.onmessage = ({ data }: MessageEvent<WorkerConsoleMessage>) => {
	const [first = "", ...rest] = data.args

	console[data.level](`[${data.where.replaceAll("%", "%%")}] ${first}`, ...rest)
}

// Lines the last test's workers posted just before it ended would otherwise be lost with the iframe.
afterAll(async () => {
	await new Promise(resolve => setTimeout(resolve, 200))

	channel.close()
})
