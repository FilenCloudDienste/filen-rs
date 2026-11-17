import { createReadStream, createWriteStream, existsSync, mkdirSync, mkdtempSync, renameSync, rmSync, statSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { Readable } from "node:stream"
import { pipeline } from "node:stream/promises"
import type { ReadableStream as NodeReadableStream } from "node:stream/web"
import { fileURLToPath } from "node:url"
import { playwright } from "@vitest/browser-playwright"
import type { Plugin } from "vite"
import { configDefaults, defineConfig, mergeConfig } from "vitest/config"
import viteConfig from "./vite.config.ts"

// The pinned RAW samples microthumb's characterisation suite keeps outside target/
// (microthumb/tests/raw_fixtures/mod.rs `cache_dir`): `<repo>/.fixture-cache/raw/<name>`.
const RAW_FIXTURE_CACHE = fileURLToPath(new URL("../../.fixture-cache/raw", import.meta.url))

// `/raw-fixtures/<name>/<getfile path>`: the pinned sample from the shared cache, fetched from
// raw.pixls.us and cached on a miss, 503 when upstream fails so the test skips rather than
// fails on a third-party outage. The browser cannot fetch the library itself — it sends no
// CORS headers and the page runs under COEP require-corp — and a bare proxy would pull 37 MB
// from a volunteer-run host on every run of every browser.
function rawFixtures(): Plugin {
	return {
		name: "raw-fixtures",
		configureServer(server) {
			server.middlewares.use("/raw-fixtures/", async (req, res) => {
				const [name, ...upstreamPath] = (req.url ?? "").replace(/^\/+/, "").split("/")

				if (!name || upstreamPath.length === 0 || name.includes("..")) {
					res.statusCode = 400
					res.end()

					return
				}

				const cached = `${RAW_FIXTURE_CACHE}/${name}`

				if (!existsSync(cached)) {
					try {
						const upstream = await fetch(`https://raw.pixls.us/getfile.php/${upstreamPath.join("/")}`)

						if (!upstream.ok || !upstream.body) {
							throw new Error(`upstream answered ${upstream.status}`)
						}

						mkdirSync(RAW_FIXTURE_CACHE, { recursive: true })

						const part = `${cached}.part`

						await pipeline(Readable.fromWeb(upstream.body as unknown as NodeReadableStream), createWriteStream(part))
						renameSync(part, cached)
					} catch (e) {
						console.warn(`raw-fixtures: ${name} unavailable`, e)

						res.statusCode = 503
						res.end()

						return
					}
				}

				res.setHeader("Content-Type", "application/octet-stream")
				res.setHeader("Content-Length", String(statSync(cached).size))
				createReadStream(cached).pipe(res)
			})
		}
	}
}

// Starts every module worker script vite serves the tests with forwardWorkerConsole
// (worker-console-shim.ts): the SDK's filen-sdk-worker-thread.js, spawned from Rust by a plain
// URL, and wasm-bindgen-rayon's workerHelpers.js, a `?worker_file` module. The service worker
// comes prebuilt from the public dir, which vite serves untransformed, so sw.ts makes the call.
const WORKER_CONSOLE_SHIM = "virtual:worker-console-shim"
const WORKER_SCRIPTS = ["/filen-sdk-worker-thread.js", "/workerHelpers.js"]

function workerConsoleShim(): Plugin {
	return {
		name: "worker-console-shim",
		resolveId(id) {
			return id === WORKER_CONSOLE_SHIM ? fileURLToPath(new URL("./worker-console-shim.ts", import.meta.url)) : undefined
		},
		transform(code, id) {
			const path = id.split("?")[0]

			if (!WORKER_SCRIPTS.some(script => path.endsWith(script))) {
				return undefined
			}

			return {
				code: `import { forwardWorkerConsole } from ${JSON.stringify(WORKER_CONSOLE_SHIM)};\nforwardWorkerConsole();\n${code}`,
				map: null
			}
		}
	}
}

// A new, empty WebKit profile for this run, removed when vitest exits.
function webkitProfile(): string {
	const dir = mkdtempSync(join(tmpdir(), "vitest-webkit-profile-"))

	process.on("exit", () => rmSync(dir, { recursive: true, force: true }))

	return dir
}

// Every test and hook gets 3 minutes, times `VITE_TEST_TIMEOUT_MULT` (default 1). The nightly
// sets 10: there each upload queues on the account-wide drive-write lock behind six native
// legs, and a cap sized for one machine says nothing (2026-09-04: a test of two tiny uploads
// took 419 s). main.test.ts reads the same variable through `import.meta.env` for the few
// explicit per-test timeouts, so set it in the shell, not in a .env file.
const TIMEOUT_MULT = Number(process.env.VITE_TEST_TIMEOUT_MULT) || 1
const TIMEOUT = 180_000 * TIMEOUT_MULT

export default defineConfig({
	...mergeConfig(viteConfig, {
		plugins: [rawFixtures(), workerConsoleShim()],
		test: {
			setupFiles: ["./worker-console-relay.setup.ts"],
			// The napi addon's tests run under plain node (`npm run test:napi`), not in a browser.
			exclude: [...configDefaults.exclude, "napi/**"],
			hookTimeout: TIMEOUT,
			testTimeout: TIMEOUT,
			teardownTimeout: 3600_000,
			browser: {
				enabled: true,
				headless: true,
				provider: playwright({
					actionTimeout: 3600_000
				}),
				instances: [
					{
						browser: "chromium"
					},
					{
						browser: "firefox"
					},
					{
						browser: "webkit",
						// A regular Playwright WebKit context is ephemeral, like a private window, and there
						// `navigator.storage.getDirectory()` rejects with UnknownError, so every test that
						// writes to OPFS would fail. A persistent context in a fresh profile has OPFS and
						// carries nothing over between runs. The provider drops `persistentContext` when the
						// instance opens more than one page, hence the single worker.
						provider: playwright({
							actionTimeout: 3600_000,
							persistentContext: webkitProfile()
						}),
						maxWorkers: 1
					}
				]
			}
		}
	})
})
