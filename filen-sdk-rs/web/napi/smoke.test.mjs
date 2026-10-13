// Offline smoke test of the Node-API addon (`npm run build:napi` first), under plain node.
// It checks what the addon exports, that errors are thrown in the shape ubrn throws them, and
// that the `filen-desktop` export condition is opt-in: no other consumer's resolution moves.

import assert from "node:assert/strict"
import { spawnSync } from "node:child_process"
import { copyFileSync, mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { dirname, join } from "node:path"
import { performance } from "node:perf_hooks"
import { after, before, describe, test } from "node:test"
import { fileURLToPath } from "node:url"

import * as sdk from "../node/index.js"

const { CacheStatusMessage_Tags, ErrorKind, FilenSdkError, JsClient, LogLevel, ResyncProgressMessage_Tags, UnauthJsClient } =
	sdk

const webDir = join(dirname(fileURLToPath(import.meta.url)), "..")
// A file URL, not a path: import() rejects an absolute Windows path (ERR_UNSUPPORTED_ESM_URL_SCHEME).
const addon = new URL("../node/index.js", import.meta.url).href

// A record that passes napi's own conversion but no SDK check: the keys cannot be parsed.
const malformedStringified = {
	email: "smoke@example.com",
	userId: 1n,
	rootUuid: "not-a-uuid",
	authInfo: "not-a-master-key",
	privateKey: "not-a-private-key",
	apiKey: "not-an-api-key",
	authVersion: 2,
	maxParallelRequests: undefined,
	maxIoMemoryUsage: undefined
}

function assertThrownLikeUbrn(e) {
	assert.ok(e instanceof Error)
	assert.equal(e.name, "Error")
	assert.equal(FilenSdkError.hasInner(e), true)
	const inner = FilenSdkError.getInner(e)
	assert.ok(FilenSdkError.instanceOf(inner))
	assert.equal(e.message, "FilenSdkError: " + inner.toDebugString())
	assert.equal(String(e), "Error: " + e.message)
	return inner
}

describe("exports", () => {
	test("are the classes, enums and functions ubrn exposes for login, the cache and the sync engine", () => {
		// napi's loader also exports each enum and class under its Rust name where that differs
		// (the *Napi mirrors, *Tags), and the binding flavor it loaded.
		assert.deepEqual(Object.keys(sdk).sort(), [
			"Backlog",
			"BacklogNapi",
			"CacheStatusMessageTags",
			"CacheStatusMessage_Tags",
			"ConflictResolution",
			"ConflictResolutionNapi",
			"DeleteGuardStateTags",
			"DeleteGuardState_Tags",
			"ErrorKind",
			"ErrorKindNapi",
			"FilenSdkError",
			"FilenSdkErrorNapi",
			"HaltReason",
			"HaltReasonNapi",
			"IgnoreLevelTags",
			"IgnoreLevel_Tags",
			"JsClient",
			"JsSyncEngine",
			"JsSyncPlan",
			"JsSyncWatch",
			"LogLevel",
			"LogLevelNapi",
			"PauseMode",
			"PauseModeNapi",
			"PlannedActionKindTags",
			"PlannedActionKind_Tags",
			"PlannedNodeKind",
			"PlannedNodeKindNapi",
			"RefuseReason",
			"RefuseReasonNapi",
			"ResyncProgressMessageTags",
			"ResyncProgressMessage_Tags",
			"SyncEventMessageTags",
			"SyncEventMessage_Tags",
			"SyncGuardReasonTags",
			"SyncGuardReason_Tags",
			"SyncMode",
			"SyncModeNapi",
			"SyncPairActivity",
			"SyncPairActivityNapi",
			"SyncRefusalTags",
			"SyncRefusal_Tags",
			"TransferDirection",
			"TransferDirectionNapi",
			"UnauthJsClient",
			"UnsyncableReasonTags",
			"UnsyncableReason_Tags",
			"WatchState",
			"WatchStateNapi",
			"__napiBindingTarget",
			"defaultSyncIgnorePatterns",
			"init",
			"setLogLevel"
		])
		assert.equal(sdk.__napiBindingTarget, "native")
		for (const name of ["FilenSdkError", "JsClient", "UnauthJsClient", "init", "setLogLevel"]) {
			assert.equal(typeof sdk[name], "function", name)
		}
		assert.equal(typeof UnauthJsClient.fromConfig, "function")
		for (const name of ["instanceOf", "hasInner", "getInner"]) {
			assert.equal(typeof FilenSdkError[name], "function", name)
		}
	})

	test("number the flat enums from 0 in declaration order", () => {
		assert.deepEqual({ ...LogLevel }, { Off: 0, Error: 1, Warn: 2, Info: 3, Debug: 4, Trace: 5 })
		// Unlike a TS enum, napi's has no reverse mapping.
		assert.equal(LogLevel[0], undefined)
		// Numbered as in the react-native build, HeifError included although the addon has no
		// heif-decoder.
		assert.deepEqual(
			{ ...ErrorKind },
			{
				Server: 0,
				Unauthenticated: 1,
				Reqwest: 2,
				Response: 3,
				RetryFailed: 4,
				Conversion: 5,
				Io: 6,
				ChunkTooLarge: 7,
				InvalidState: 8,
				InvalidType: 9,
				InvalidName: 10,
				ImageError: 11,
				MetadataWasNotDecrypted: 12,
				Cancelled: 13,
				HeifError: 14,
				BadRecoveryKey: 15,
				Internal: 16,
				InsufficientMemory: 17,
				Walk: 18,
				FileChangedDuringSync: 19,
				FolderNotFound: 20,
				WrongPassword: 21,
				MaxStorageReached: 22,
				FileChunkNotFound: 23,
				FileNotFound: 24,
				EmailOrPasswordWrong: 25,
				Enter2fa: 26,
				Wrong2fa: 27,
				StaleState: 28,
				MissingStableUuid: 29,
				ArchiveCorrupt: 30,
				ArchiveUnsupported: 31,
				ArchiveTooLarge: 32,
				ArchiveWorkerDied: 33,
				ArchivePasswordRequired: 34,
				ArchiveWrongPassword: 35,
				ArchiveSolidSkipExceeded: 36,
				LockLost: 37
			}
		)
	})

	test("name the data enums' tags as strings", () => {
		assert.deepEqual(
			{ ...CacheStatusMessage_Tags },
			{ Errors: "Errors", SyncRootsDeleted: "SyncRootsDeleted", ResyncProgress: "ResyncProgress" }
		)
		assert.deepEqual(
			{ ...ResyncProgressMessage_Tags },
			{ Started: "Started", Listing: "Listing", Applying: "Applying", Finished: "Finished" }
		)
	})

	test("init and setLogLevel run", () => {
		sdk.init()
		sdk.setLogLevel(LogLevel.Warn)
	})
})

describe("UnauthJsClient", () => {
	test("fromConfig({}) builds a client", t => {
		const start = performance.now()
		const client = UnauthJsClient.fromConfig({})
		t.diagnostic(`fromConfig({}) took ${(performance.now() - start).toFixed(1)} ms`)
		assert.ok(client instanceof UnauthJsClient)
		assert.equal(typeof client.login, "function")
		assert.equal(typeof client.fromStringified, "function")
	})

	test("fromConfig throws a FilenSdkError the way ubrn throws it", () => {
		// Upload limits in 1..16 KB/s are rejected at construction.
		assert.throws(
			() => UnauthJsClient.fromConfig({ uploadBandwidthKilobytesPerSec: 5 }),
			e => {
				const inner = assertThrownLikeUbrn(e)
				assert.equal(inner.kind(), ErrorKind.InvalidState)
				return true
			}
		)
	})

	test("fromStringified throws a FilenSdkError for keys it cannot parse", () => {
		const client = UnauthJsClient.fromConfig({})
		assert.throws(
			() => client.fromStringified(malformedStringified),
			e => {
				assert.equal(assertThrownLikeUbrn(e).kind(), ErrorKind.Conversion)
				return true
			}
		)
	})

	test("a number where a bigint belongs throws napi's own error, not a FilenSdkError", () => {
		const client = UnauthJsClient.fromConfig({})
		assert.throws(
			() => client.fromStringified({ ...malformedStringified, userId: 1 }),
			e => {
				assert.ok(e instanceof Error)
				assert.equal(e.code, "BigintExpected")
				assert.equal(FilenSdkError.hasInner(e), false)
				return true
			}
		)
	})

	test("a mistyped argument to an async method throws instead of rejecting", () => {
		// ubrn's async methods convert their arguments inside the promise and reject; napi
		// converts them before the method runs.
		const client = UnauthJsClient.fromConfig({})
		assert.throws(
			() => client.login({ email: 1, password: "not-a-password", twoFactorCode: undefined }),
			e => {
				assert.ok(e instanceof Error)
				assert.equal(e.code, "StringExpected")
				assert.equal(FilenSdkError.hasInner(e), false)
				return true
			}
		)
	})

	test("login with an already aborted signal rejects with an AbortError", async () => {
		const client = UnauthJsClient.fromConfig({})
		await assert.rejects(
			client.login(
				{ email: "smoke@example.com", password: "not-a-password", twoFactorCode: undefined },
				{ signal: AbortSignal.abort() }
			),
			e => {
				assert.equal(e.name, "AbortError")
				assert.equal(e.message, "A Rust future was aborted")
				assert.equal(FilenSdkError.hasInner(e), false)
				return true
			}
		)
	})

	test("hasInner is false for values ubrn would not have thrown", () => {
		for (const value of [undefined, null, 1, "FilenSdkError", new Error("plain"), { inner: {} }]) {
			assert.equal(FilenSdkError.hasInner(value), false)
		}
	})
})

test("a process that used the addon exits by itself", () => {
	const script = `
		const { UnauthJsClient } = await import(${JSON.stringify(addon)})
		const client = UnauthJsClient.fromConfig({})
		await client
			.login({ email: "a@example.com", password: "p", twoFactorCode: undefined }, { signal: AbortSignal.abort() })
			.catch(() => {})
	`
	const run = spawnSync(process.execPath, ["--input-type=module", "-e", script], { encoding: "utf8", timeout: 30_000 })
	assert.equal(run.error, undefined, "the process did not exit within 30 s")
	assert.equal(run.status, 0, run.stderr)
})

describe("package resolution", () => {
	// The resolver checks that a target exists, and the wasm and react-native outputs are not
	// built here, so resolve in a copy of package.json beside empty placeholder targets.
	let dir

	before(() => {
		dir = realpathSync(mkdtempSync(join(tmpdir(), "filen-sdk-rs-exports-")))
		copyFileSync(join(webDir, "package.json"), join(dir, "package.json"))
		for (const target of ["sdk-rs.js", "sdk-rs.d.ts", "node/index.js", "node/index.d.ts", "src/index.tsx"]) {
			mkdirSync(dirname(join(dir, target)), { recursive: true })
			writeFileSync(join(dir, target), "")
		}
	})

	after(() => rmSync(dir, { recursive: true, force: true }))

	function resolve(conditions) {
		const run = spawnSync(
			process.execPath,
			[...conditions.map(c => `--conditions=${c}`), "--input-type=module", "-e", 'console.log(import.meta.resolve("@filen/sdk-rs"))'],
			{ cwd: dir, encoding: "utf8" }
		)
		assert.equal(run.status, 0, run.stderr)
		return fileURLToPath(run.stdout.trim())
	}

	test("default conditions resolve to the wasm build", () => {
		assert.equal(resolve([]), join(dir, "sdk-rs.js"))
	})

	test("the filen-desktop condition resolves to the addon's loader", () => {
		assert.equal(resolve(["filen-desktop"]), join(dir, "node", "index.js"))
	})

	test("the filen-desktop condition wins over browser and react-native", () => {
		assert.equal(resolve(["filen-desktop", "browser", "react-native"]), join(dir, "node", "index.js"))
	})

	test("browser resolves to the wasm build", () => {
		assert.equal(resolve(["browser"]), join(dir, "sdk-rs.js"))
	})

	test("react-native resolves to the react-native sources", () => {
		assert.equal(resolve(["react-native"]), join(dir, "src", "index.tsx"))
	})
})
