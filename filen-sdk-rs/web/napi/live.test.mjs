// Live test of the Node-API addon (`npm run build:napi` first): it logs in to the test account and
// drives a real sync between a temp folder and a fresh remote directory, through the addon only,
// the way the desktop app does, then serves the account over webdav from a real rclone
// (FILEN_TEST_RCLONE, as rclone_daemon_tests take it; that part is skipped without it). Needs TEST_EMAIL / TEST_PASSWORD, from the environment or from the
// nearest dotenv file up from this package, as the Rust tests read them; skipped without them.
//
// The addon has no API to create a remote directory, so test-utils' `remote-test-dir` lends one
// (built by `cargo run` here): it prints the directory's uuid and deletes the directory when its
// stdin closes, which happens when this process ends however it ends. Every pass the engine runs
// takes the account's drive-write lock itself, as do the directory's creation and deletion; run
// this alone on the account, like the Rust live suites.

import assert from "node:assert/strict"
import { spawn } from "node:child_process"
import { randomBytes, randomUUID } from "node:crypto"
import { once } from "node:events"
import { existsSync, mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs"
import { createServer } from "node:net"
import { tmpdir } from "node:os"
import { dirname, join } from "node:path"
import { performance } from "node:perf_hooks"
import { createInterface } from "node:readline"
import { after, before, describe, test } from "node:test"
import { setTimeout as sleep } from "node:timers/promises"
import { fileURLToPath } from "node:url"

import {
	ErrorKind,
	FilenSdkError,
	IgnoreLevel_Tags,
	PlannedActionKind_Tags,
	ServeAuth_Tags,
	ServeProtocol,
	SyncEventMessage_Tags,
	SyncMode,
	UnauthJsClient,
	WatchState
} from "../node/index.js"

const webDir = join(dirname(fileURLToPath(import.meta.url)), "..")

if (!process.env.TEST_EMAIL) {
	for (let dir = webDir; ; dir = dirname(dir)) {
		const file = join(dir, ".env")
		if (existsSync(file)) {
			process.loadEnvFile(file)
			break
		}
		if (dirname(dir) === dir) {
			break
		}
	}
}
const { TEST_EMAIL: email, TEST_PASSWORD: password } = process.env

// How long a pass, or the cache's resync, may wait for the drive-write lock: the Rust tests'
// CACHE_CONVERGE_TIMEOUT.
const CONVERGE_TIMEOUT_MS = 240_000

/** Poll `probe` until it returns something truthy, and return that. */
async function until(what, probe, timeoutMs = CONVERGE_TIMEOUT_MS) {
	const deadline = performance.now() + timeoutMs
	for (;;) {
		const found = probe()
		if (found) {
			return found
		}
		assert.ok(performance.now() < deadline, `${what}: not within ${timeoutMs / 1000} s`)
		await sleep(50)
	}
}

/**
 * Start `remote-test-dir`. It is returned before it lends anything, so whatever happens next can end
 * it; `uuid` resolves to the directory it prints, or to undefined if it exits first.
 */
function lendRemoteDir() {
	const helper = spawn(
		"cargo",
		["run", "--quiet", "--manifest-path", join(webDir, "..", "..", "test-utils", "Cargo.toml"), "--bin", "remote-test-dir"],
		{ stdio: ["pipe", "pipe", "inherit"], env: { ...process.env, FILEN_TEST_LOG_STDERR: "1", RUST_LOG: "warn" } }
	)
	const exited = once(helper, "exit")
	// readline keeps reading after the first line, so the helper never blocks on a full pipe.
	const firstLine = once(createInterface({ input: helper.stdout }), "line").then(([line]) => line)
	return { helper, exited, uuid: Promise.race([firstLine, exited.then(() => undefined)]) }
}

const kind = e => FilenSdkError.getInner(e).kind()

describe("the addon against the server", { skip: email && password ? false : "TEST_EMAIL / TEST_PASSWORD are unset" }, () => {
	const cacheMessages = []
	const events = []
	const statuses = []
	// Every call must come bound to the listener, and the first status throws: the addon logs it
	// and keeps delivering, and nothing reaches the process as an uncaught exception.
	const unbound = []
	const uncaught = []
	const collect = e => uncaught.push(e)
	let threw = false
	const listener = {
		onEvents(batch) {
			if (this !== listener) {
				unbound.push("onEvents")
			}
			events.push(...batch)
		},
		onWatchStatus(pairId, status) {
			if (this !== listener) {
				unbound.push("onWatchStatus")
			}
			statuses.push({ pairId, status })
			if (!threw) {
				threw = true
				throw new Error("listener boom")
			}
		}
	}
	let lent, remoteRoot, dir, local, engine, pair

	const write = (relPath, text) => {
		const path = join(local, relPath)
		mkdirSync(dirname(path), { recursive: true })
		writeFileSync(path, text)
	}

	/** Pass `passId`'s events, in delivery order. */
	const pass = passId => events.filter(event => event.passId === passId)

	/** The first PassCompleted delivered at index `from` or later whose report `accept` takes. */
	const completed = (from, accept = () => true) =>
		until("a matching pass completed", () =>
			events.slice(from).find(event => event.event.tag === SyncEventMessage_Tags.PassCompleted && accept(event.event.inner.report))
		)

	before(
		async () => {
			process.on("uncaughtException", collect)
			lent = lendRemoteDir()
			remoteRoot = await lent.uuid
			assert.ok(remoteRoot, "remote-test-dir exited before lending a directory")
			dir = realpathSync.native(mkdtempSync(join(tmpdir(), "filen-sdk-rs-live-")))
			local = join(dir, "local")
			mkdirSync(local)
			const client = await UnauthJsClient.fromConfig({}).login({ email, password, twoFactorCode: undefined })
			await client.configureCache(join(dir, "cache.db"), {
				onMessages(messages) {
					cacheMessages.push(...messages)
				}
			})
			engine = await client.openSyncEngine(join(dir, "baseline.db"), listener)
			pair = await engine.addPair(local, remoteRoot, SyncMode.TwoWay)
			// The first pass plans against the cache: wait for a resync of the new directory to
			// converge, as the Rust tests do. Resyncs run one at a time, so the Finished after a
			// Started is that resync's.
			await until("the cache converged on the test directory", () => {
				let coversRoot = false
				for (const message of cacheMessages) {
					if (message.tag !== "ResyncProgress") {
						continue
					}
					const { progress } = message.inner
					if (progress.tag === "Started") {
						coversRoot = progress.inner.roots.includes(remoteRoot)
					} else if (progress.tag === "Finished" && progress.inner.converged && coversRoot) {
						return true
					}
				}
				return false
			})
		},
		// The first `cargo run` builds the helper.
		{ timeout: 30 * 60_000 }
	)

	// Each step runs whatever the one before it did. The helper's pipes keep this process alive, so
	// it must be ended however the rest went: `close`, which waits out a transfer in flight (up to
	// the 300 s read timeout), is given up on well inside the hook's timeout.
	after(
		async () => {
			process.off("uncaughtException", collect)
			try {
				const gaveUp = sleep(6 * 60_000, undefined, { ref: false }).then(() => assert.fail("close took over 6 minutes"))
				await Promise.race([engine?.close(), gaveUp])
			} finally {
				try {
					if (dir) {
						// The baseline's SQLite files may be held for a moment after `close` on Windows.
						rmSync(dir, { recursive: true, force: true, maxRetries: 10 })
					}
				} finally {
					if (lent) {
						lent.helper.stdin.end()
						const [code] = await lent.exited
						assert.equal(code, 0, "remote-test-dir failed; see its output above")
					}
				}
			}
		},
		{ timeout: 10 * 60_000 }
	)

	test("a dry run is read a page at a time, names what the user ignores and releases", async () => {
		write("a.txt", "alpha")
		write("docs/b.txt", "bravo!")
		write("skip.tmp", "scratch")
		await engine.setUserIgnore("*.tmp")
		assert.equal(await engine.userIgnore(), "*.tmp")

		const plan = await engine.planPair(pair)
		const summary = plan.summary()
		assert.equal(summary.actionsTotal, 3n)
		assert.equal(summary.uploadBytes, 5n + 6n)
		assert.deepEqual([summary.ignoredTotal, summary.conflictsTotal, summary.heldTotal], [1n, 0n, 0n])
		const first = plan.actions(0n, 2n)
		const rest = plan.actions(2n, 2n)
		assert.deepEqual([first.length, rest.length], [2, 1])
		assert.deepEqual(plan.actions(3n, 2n), [], "a page past the end")
		assert.deepEqual(
			[...first, ...rest].map(action => `${action.kind.tag} ${action.relPath}`).sort(),
			[
				`${PlannedActionKind_Tags.CreateRemoteDir} docs`,
				`${PlannedActionKind_Tags.UploadFile} a.txt`,
				`${PlannedActionKind_Tags.UploadFile} docs/b.txt`
			].sort()
		)
		const ignored = plan.ignored(0n, 10n)
		assert.deepEqual(
			ignored.map(path => [path.relPath, path.level.tag, path.pattern]),
			[["skip.tmp", IgnoreLevel_Tags.User, "*.tmp"]]
		)
		plan.release()
		assert.deepEqual(plan.actions(0n, 10n), [], "a released plan keeps a list")
		assert.deepEqual(plan.summary(), summary, "release changed the summary")
	})

	test("a one-shot pass is announced in order under one pass id and reported", async () => {
		const from = events.length
		const report = await engine.syncOnce(pair)
		assert.deepEqual([report.uploaded, report.remoteDirsCreated, report.ignoredTotal], [2n, 1n, 1n])
		assert.deepEqual(report.errors, [])
		assert.equal(report.passError, undefined)

		const done = await completed(from)
		const passEvents = pass(done.passId)
		assert.ok(
			passEvents.every(event => event.pairId === pair),
			"an event of another pair"
		)
		assert.deepEqual(passEvents[0].event, { tag: SyncEventMessage_Tags.PassStarted, inner: { mode: SyncMode.TwoWay } })
		assert.deepEqual(passEvents.at(-1).event, { tag: SyncEventMessage_Tags.PassCompleted, inner: { report } })
		const tags = passEvents.map(event => event.event.tag)
		const planned = passEvents.findIndex(event => event.event.tag === SyncEventMessage_Tags.Planned)
		assert.equal(passEvents[planned]?.event.inner.actions, 3n, "the pass never said what it planned")
		assert.ok(planned < tags.indexOf(SyncEventMessage_Tags.Uploading), tags.join(", "))
		const paths = tag =>
			passEvents
				.filter(event => event.event.tag === tag)
				.map(event => event.event.inner.relPath)
				.sort()
		assert.deepEqual(paths(SyncEventMessage_Tags.Uploading), ["a.txt", "docs/b.txt"])
		assert.deepEqual(paths(SyncEventMessage_Tags.CreatingRemoteDir), ["docs"])

		// What a screen mounted after the pass reads back.
		assert.deepEqual(await engine.lastReport(pair), report)
		const recent = await engine.recentEvents()
		assert.ok(recent.some(event => event.passId === done.passId && event.event.tag === SyncEventMessage_Tags.PassStarted))
		const [info] = await engine.listPairs()
		assert.deepEqual([info.id, info.remoteRoot, info.paused, info.passInFlight], [pair, remoteRoot, false, undefined])
		assert.equal(typeof info.lastPassCompletedAtMs, "bigint")
	})

	test("a watch syncs a local change and a requested pass, then reports Stopped", async () => {
		const uploadsOne = report => report.uploaded === 1n
		write("first.txt", "first")
		let from = events.length
		const watch = await engine.watchPair(pair, undefined)
		assert.equal(watch.pairId(), pair)
		await completed(from, uploadsOne)

		// While watched, the pair takes no one-shot pass.
		await assert.rejects(engine.syncOnce(pair), e => kind(e) === ErrorKind.InvalidState)

		// A file written into the local root is picked up by the watcher, with no call.
		from = events.length
		write("second.txt", "second")
		await completed(from, uploadsOne)

		// A requested pass reads both sides whole.
		from = events.length
		await engine.requestPass(pair)
		const requested = await completed(from, report => report.fullPassReason === "a caller asked for a pass")
		assert.deepEqual([requested.event.inner.report.errors, requested.event.inner.report.uploaded], [[], 0n])

		assert.ok(
			events.every(event => event.pairId === pair),
			"an event of another pair"
		)
		const started = events.filter(event => event.event.tag === SyncEventMessage_Tags.PassStarted).map(event => event.passId)
		assert.ok(started.length >= 4, `${started}`)
		assert.ok(
			started.every((id, i) => i === 0 || started[i - 1] < id),
			`pass ids must increase: ${started}`
		)

		await watch.stop()
		assert.equal(watch.status().state, WatchState.Stopped)
		await until("the listener heard the watch stop", () => statuses.at(-1)?.status.state === WatchState.Stopped)
		assert.ok(
			statuses.every(({ pairId }) => pairId === pair),
			"a status of another pair"
		)
		assert.equal(statuses[0].status.state, WatchState.Running)
		assert.ok(threw)
		assert.deepEqual(uncaught, [])
		assert.deepEqual(unbound, [])
		assert.equal((await engine.listPairs())[0].watchStatus, undefined, "a stopped watch still shows on its pair")
	})

	test("close ends the engine", async () => {
		await engine.close()
		await assert.rejects(engine.listPairs(), e => {
			assert.equal(kind(e), ErrorKind.InvalidState)
			assert.match(FilenSdkError.getInner(e).toDebugString(), /the sync engine is closed/)
			return true
		})
	})
})

/** A loopback port no one listens on now. */
async function freePort() {
	const server = createServer()
	server.listen(0, "127.0.0.1")
	await once(server, "listening")
	const { port } = server.address()
	server.close()
	await once(server, "close")
	return port
}

describe(
	"the rclone daemon against the server",
	{
		skip: !(email && password) ? "TEST_EMAIL / TEST_PASSWORD are unset" : process.env.FILEN_TEST_RCLONE ? false : "FILEN_TEST_RCLONE is unset"
	},
	() => {
		let dir, daemon

		before(async () => {
			dir = realpathSync.native(mkdtempSync(join(tmpdir(), "filen-sdk-rs-live-rclone-")))
			const client = await UnauthJsClient.fromConfig({}).login({ email, password, twoFactorCode: undefined })
			daemon = await client.startRcloneDaemon({
				binaryPath: process.env.FILEN_TEST_RCLONE,
				binarySha256: undefined,
				stateDir: join(dir, "state"),
				cacheDir: join(dir, "cache"),
				logFile: join(dir, "rclone.log"),
				userAgent: "filen-sdk-rs-live",
				vfs: { cacheMaxSize: 1n << 30n, cacheMinFreeSpace: 1n << 30n, cacheMaxAgeSecs: 3600n, dirCacheTimeSecs: 15 }
			})
		})

		after(
			async () => {
				try {
					await daemon?.stop({ drainTimeoutMs: 60_000, graceMs: 10_000 })
				} finally {
					if (dir) {
						rmSync(dir, { recursive: true, force: true, maxRetries: 10 })
					}
				}
			},
			{ timeout: 2 * 60_000 }
		)

		test("a loopback webdav serve with random credentials round-trips a file and refuses a request without them", async () => {
			const user = `filen-${randomBytes(4).toString("hex")}`
			const pass = randomBytes(32).toString("base64url")
			const serve = await daemon.startServe({
				protocol: ServeProtocol.Webdav,
				host: "127.0.0.1",
				port: await freePort(),
				auth: { tag: ServeAuth_Tags.Basic, inner: { user, password: pass } },
				tls: undefined,
				readOnly: false
			})
			const base = `http://${serve.addr}/filen-sdk-rs-napi-rclone-${randomUUID()}`
			const authorization = `Basic ${Buffer.from(`${user}:${pass}`).toString("base64")}`
			const request = (path, init = {}) => fetch(`${base}${path}`, { ...init, headers: { authorization, ...init.headers } })

			assert.equal((await fetch(`http://${serve.addr}/`)).status, 401)
			assert.equal((await request("", { method: "MKCOL" })).status, 201)
			try {
				assert.equal((await request("/file.txt", { method: "PUT", body: "round trip" })).status, 201)
				// Uploaded once the VFS cache's write-back delay (5 s) has passed.
				const deadline = performance.now() + 120_000
				for (;;) {
					const stats = await daemon.stats()
					if (stats.uploadsInProgress + stats.uploadsQueued === 0) {
						break
					}
					assert.ok(performance.now() < deadline, "the file was not uploaded within 120 s")
					await sleep(500)
				}
				const read = await request("/file.txt")
				assert.equal(read.status, 200)
				assert.equal(await read.text(), "round trip")
				assert.deepEqual(
					(await daemon.stats()).serves.map(({ id, protocol }) => [id, protocol]),
					[[serve.id, ServeProtocol.Webdav]]
				)
			} finally {
				assert.equal((await request("", { method: "DELETE" })).status, 204)
				await daemon.stopServe(serve.id)
			}
		})
	}
)
