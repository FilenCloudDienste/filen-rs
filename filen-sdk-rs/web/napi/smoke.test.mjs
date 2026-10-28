// Offline smoke test of the Node-API addon (`npm run build:napi` first), under plain node.
// It checks what the addon exports, that errors are thrown in the shape ubrn throws them, and
// that the `filen-desktop` export condition is opt-in: no other consumer's resolution moves.

import assert from "node:assert/strict"
import { spawnSync } from "node:child_process"
import { generateKeyPairSync, randomUUID } from "node:crypto"
import { once } from "node:events"
import { copyFileSync, mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { dirname, join } from "node:path"
import { performance } from "node:perf_hooks"
import { DatabaseSync } from "node:sqlite"
import { after, before, describe, test } from "node:test"
import { fileURLToPath } from "node:url"
import { Worker } from "node:worker_threads"

import * as sdk from "../node/index.js"

const {
	Backlog,
	CacheStatusMessage_Tags,
	ConflictResolution,
	DeleteGuardState_Tags,
	ErrorKind,
	FilenSdkError,
	FuseProvider,
	FuseStatus_Tags,
	HaltReason,
	JsClient,
	JsRcloneDaemon,
	JsSyncEngine,
	JsSyncPlan,
	JsSyncWatch,
	LogLevel,
	PauseMode,
	PlannedNodeKind,
	RcloneRefusal_Tags,
	RefuseReason,
	ResyncProgressMessage_Tags,
	ServeProtocol,
	SyncMode,
	SyncPairActivity,
	SyncRefusal_Tags,
	TransferDirection,
	UnauthJsClient,
	WatchState
} = sdk

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

// A V2 client that parses offline, as `auth::test_support::offline_client_with_root` builds it.
// 1024-bit rather than the Rust tests' 512: OpenSSL 3 setups may refuse 512.
function synthStringified(rootUuid) {
	const privateKey = generateKeyPairSync("rsa", { modulusLength: 1024 })
		.privateKey.export({ type: "pkcs8", format: "der" })
		.toString("base64")
	return {
		email: "smoke@example.com",
		userId: 1n,
		rootUuid,
		authInfo: "0".repeat(64),
		privateKey,
		apiKey: "",
		authVersion: 2,
		maxParallelRequests: undefined,
		maxIoMemoryUsage: undefined
	}
}

// The engine stores `std::fs::canonicalize`: on Windows a verbatim `\\?\` path with 8.3 short
// names expanded, where `realpathSync.native` strips the prefix.
const canon = p => (process.platform === "win32" ? p.replace(/^\\\\\?\\/, "").toLowerCase() : p)

const tags = names => Object.fromEntries(names.map(name => [name, name]))

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
	test("are the classes, enums and functions ubrn exposes for login, the cache, the sync engine and rclone", () => {
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
			"FuseProvider",
			"FuseProviderNapi",
			"FuseStatusTags",
			"FuseStatus_Tags",
			"HaltReason",
			"HaltReasonNapi",
			"IgnoreLevelTags",
			"IgnoreLevel_Tags",
			"JsClient",
			"JsRcloneDaemon",
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
			"RcloneExitTags",
			"RcloneExit_Tags",
			"RcloneRefusalTags",
			"RcloneRefusal_Tags",
			"RefuseReason",
			"RefuseReasonNapi",
			"ResyncProgressMessageTags",
			"ResyncProgressMessage_Tags",
			"ServeAuthTags",
			"ServeAuth_Tags",
			"ServeProtocol",
			"ServeProtocolNapi",
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
			"rcloneAvailableDriveLetters",
			"rcloneClearCache",
			"rcloneForceUnmount",
			"rcloneFuseStatus",
			"rcloneRecoverStale",
			"rcloneStopUnder",
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

	test("number the sync engine's flat enums from 0 in declaration order", () => {
		assert.deepEqual({ ...SyncMode }, { LocalToRemote: 0, RemoteToLocal: 1, TwoWay: 2, LocalBackup: 3, RemoteBackup: 4 })
		assert.deepEqual({ ...Backlog }, { Propagate: 0, AdoptDestination: 1 })
		assert.deepEqual({ ...PauseMode }, { Suspend: 0, Cancel: 1 })
		assert.deepEqual({ ...ConflictResolution }, { KeepLocal: 0, KeepRemote: 1, KeepBoth: 2 })
		assert.deepEqual({ ...TransferDirection }, { Upload: 0, Download: 1 })
		assert.deepEqual({ ...RefuseReason }, { RemoteCollision: 0, LocalCollision: 1 })
		assert.deepEqual({ ...HaltReason }, { LocalStorageFull: 0, RemoteStorageFull: 1 })
		assert.deepEqual({ ...PlannedNodeKind }, { Dir: 0, File: 1 })
		assert.deepEqual({ ...WatchState }, { Running: 0, Paused: 1, Stopped: 2, PairRemoved: 3 })
		assert.deepEqual({ ...SyncPairActivity }, { Idle: 0, Starting: 1, Watching: 2, Stopping: 3, SyncingOnce: 4 })
	})

	test("name the sync engine's data enums' tags as strings", () => {
		assert.deepEqual({ ...DeleteGuardState_Tags }, tags(["Limited", "Unlimited"]))
		assert.deepEqual({ ...sdk.IgnoreLevel_Tags }, tags(["Default", "User", "File"]))
		assert.deepEqual(
			{ ...sdk.PlannedActionKind_Tags },
			tags([
				"UploadFile",
				"DownloadFile",
				"CreateRemoteDir",
				"CreateLocalDir",
				"MoveRemote",
				"MoveLocal",
				"TrashRemote",
				"DeleteLocal",
				"AdoptBaseline"
			])
		)
		assert.deepEqual(
			{ ...sdk.SyncEventMessage_Tags },
			tags([
				"PassStarted",
				"Refused",
				"Planned",
				"Conflict",
				"DeletionsHeld",
				"Uploading",
				"Downloading",
				"Progress",
				"CreatingRemoteDir",
				"CreatingLocalDir",
				"TrashingRemote",
				"DeletingLocal",
				"MovingRemote",
				"MovingLocal",
				"Quarantined",
				"ActionFailed",
				"Interrupted",
				"AdoptedBaseline",
				"PassCompleted",
				"PassFailed",
				"AwaitingRemote"
			])
		)
		assert.deepEqual(
			{ ...sdk.SyncGuardReason_Tags },
			tags(["ScanIncomplete", "FirstSyncWithDeletions", "RemoteUnconverged", "RemoteEmptied", "ExceededThreshold"])
		)
		assert.deepEqual(
			{ ...SyncRefusal_Tags },
			tags([
				"BaselineInUse",
				"BaselineSchemaMismatch",
				"LocalRootInUse",
				"LocalRootNested",
				"LocalRootContains",
				"RemoteRootInUse",
				"RemoteRootNested",
				"RemoteRootContains"
			])
		)
		assert.deepEqual(
			{ ...sdk.UnsyncableReason_Tags },
			tags(["InvalidName", "RepeatedFailure", "RemoteUndecodable", "RemoteInvalidName", "RemoteBrokenParent", "LocalAlias"])
		)
	})

	test("the sync engine classes carry ubrn's methods", () => {
		const methods = cls => {
			assert.equal(typeof cls, "function")
			return Object.getOwnPropertyNames(cls.prototype)
				.filter(name => name !== "constructor")
				.sort()
		}
		assert.deepEqual(methods(JsSyncEngine), [
			"addPair",
			"approveDeletions",
			"cancelPausedActions",
			"close",
			"importLegacyState",
			"lastReport",
			"listConflicts",
			"listPairs",
			"pausePair",
			"planPair",
			"recentEvents",
			"reconfigurePair",
			"removePair",
			"requestPass",
			"resolveConflict",
			"resumePair",
			"retryPath",
			"setDeleteGuard",
			"setDeleteGuardUnlimited",
			"setUserIgnore",
			"stopWatch",
			"syncOnce",
			"userIgnore",
			"watchPair"
		])
		assert.deepEqual(methods(JsSyncWatch), ["pairId", "status", "stop"])
		assert.deepEqual(methods(JsSyncPlan), ["actions", "conflicts", "held", "ignored", "release", "summary", "unsyncable"])
		assert.equal(typeof JsClient.prototype.openSyncEngine, "function")
	})

	test("number rclone's flat enums from 0 in declaration order", () => {
		assert.deepEqual({ ...FuseProvider }, { MacFuse: 0, NfsMount: 1, WinFsp: 2, Fuse3: 3 })
		assert.deepEqual({ ...ServeProtocol }, { Webdav: 0, S3: 1, Ftp: 2, Sftp: 3, Http: 4, Restic: 5, Nfs: 6 })
	})

	test("name rclone's data enums' tags as strings", () => {
		assert.deepEqual({ ...FuseStatus_Tags }, tags(["Ready", "Missing"]))
		assert.deepEqual({ ...sdk.RcloneExit_Tags }, tags(["Stopped", "Crashed"]))
		assert.deepEqual({ ...sdk.ServeAuth_Tags }, tags(["Basic", "S3", "Unauthenticated"]))
		assert.deepEqual(
			{ ...RcloneRefusal_Tags },
			tags([
				"BinaryMissing",
				"BinaryChecksum",
				"UnsupportedVersion",
				"WatchdogUnavailable",
				"FuseMissing",
				"MountPointInUse",
				"MountPointNotEmpty",
				"MountPointInvalid",
				"DriveLetterInUse",
				"PortInUse",
				"AddressNotLocal",
				"ImplicitAddress",
				"AuthRequired",
				"InvalidCredentials",
				"UnauthenticatedOffLoopback",
				"UnsupportedProtocol",
				"TlsUnsupported",
				"UploadsPending",
				"CacheInUse"
			])
		)
	})

	test("the rclone daemon class carries ubrn's methods", () => {
		assert.deepEqual(Object.getOwnPropertyNames(JsRcloneDaemon.prototype).filter(name => name !== "constructor").sort(), [
			"exited",
			"mount",
			"pid",
			"serveTypes",
			"startServe",
			"stats",
			"stop",
			"stopServe",
			"unmount"
		])
		assert.equal(typeof JsClient.prototype.startRcloneDaemon, "function")
	})

	test("defaultSyncIgnorePatterns lists the OS clutter", () => {
		const patterns = sdk.defaultSyncIgnorePatterns()
		assert.ok(Array.isArray(patterns) && patterns.length > 0)
		assert.ok(patterns.every(p => typeof p === "string"))
		assert.ok(patterns.includes(".DS_Store"))
		assert.ok(patterns.includes("Thumbs.db"))
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
				// The sync-engine impl block lands on FilenSdkError too.
				assert.equal(typeof inner.syncRefusal, "function")
				assert.equal(inner.syncRefusal(), undefined)
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

// Temp dirs for the sync engine tests; on Windows the baseline's SQLite files may still be held
// for a moment after `close`, whose teardown finishes on the commander runtime.
function tempDir(prefix) {
	return realpathSync.native(mkdtempSync(join(tmpdir(), prefix)))
}
const removeDir = dir => rmSync(dir, { recursive: true, force: true, maxRetries: 10 })

// A client of a synthesized V2 account, with no cache configured.
const synthClient = rootUuid => UnauthJsClient.fromConfig({}).fromStringified(synthStringified(rootUuid))

// A worker_thread that opens an engine on `dbPath`, adds a pair beside it and watches it, offline
// as above, then runs until it is terminated (an engine does not keep the event loop alive). It
// keeps the handles reachable, so only the end of its Node environment lets the engine go. It
// posts "watching" once the watch runs.
function watchInWorker(dbPath, rootUuid) {
	const source = `
		const { parentPort, workerData } = require("node:worker_threads")
		const { mkdirSync } = require("node:fs")
		setInterval(() => {}, 1 << 30)
		import(workerData.addon).then(async ({ UnauthJsClient }) => {
			const { dbPath, twoWay } = workerData
			const client = UnauthJsClient.fromConfig({}).fromStringified(workerData.stringified)
			globalThis.engine = await client.openSyncEngine(dbPath, { onEvents() {}, onWatchStatus() {} })
			const local = dbPath + ".local"
			mkdirSync(local)
			const id = await engine.addPair(local, crypto.randomUUID(), twoWay)
			globalThis.watch = await engine.watchPair(id, undefined)
			parentPort.postMessage("watching")
		})
	`
	return new Worker(source, {
		eval: true,
		workerData: { addon, dbPath, twoWay: SyncMode.TwoWay, stringified: synthStringified(rootUuid) }
	})
}

describe("rclone, offline", () => {
	let dir

	before(() => {
		dir = tempDir("filen-sdk-rs-rclone-")
	})

	after(() => removeDir(dir))

	test("rcloneFuseStatus names this system's mount backend", async () => {
		const status = await sdk.rcloneFuseStatus()
		if (process.platform === "darwin") {
			// macOS always has one: macFUSE when installed, rclone's nfsmount otherwise.
			assert.equal(status.tag, FuseStatus_Tags.Ready)
			assert.ok([FuseProvider.MacFuse, FuseProvider.NfsMount].includes(status.inner.provider), `${status.inner.provider}`)
		} else {
			assert.ok([FuseStatus_Tags.Ready, FuseStatus_Tags.Missing].includes(status.tag), status.tag)
		}
	})

	test("rcloneAvailableDriveLetters lists free letters on Windows only", async () => {
		const letters = await sdk.rcloneAvailableDriveLetters()
		if (process.platform === "win32") {
			assert.ok(letters.every(letter => /^[D-Z]$/.test(letter)), `${letters}`)
		} else {
			assert.deepEqual(letters, [])
		}
	})

	test("rcloneRecoverStale on a fresh state directory recovers nothing", async () => {
		assert.deepEqual(await sdk.rcloneRecoverStale(join(dir, "state")), { stopped: false, unmounted: [] })
	})

	test("rcloneStopUnder a missing directory stops nothing", async () => {
		assert.equal(await sdk.rcloneStopUnder(join(dir, "missing"), 1000), 0)
	})

	test("a cache with an upload pending is refused with an rclone refusal", async () => {
		const meta = join(dir, "cache", "vfsMeta", "Filen")
		mkdirSync(meta, { recursive: true })
		writeFileSync(join(meta, "file.txt"), JSON.stringify({ Size: 7, Dirty: true }))
		await assert.rejects(sdk.rcloneClearCache(join(dir, "state"), join(dir, "cache")), e => {
			const inner = assertThrownLikeUbrn(e)
			assert.equal(inner.kind(), ErrorKind.Io)
			assert.deepEqual(inner.rcloneRefusal(), { tag: RcloneRefusal_Tags.UploadsPending, inner: { count: 1 } })
			assert.equal(inner.syncRefusal(), undefined)
			return true
		})
	})

	test("a daemon whose binary is missing is refused with BinaryMissing", async () => {
		const client = synthClient(randomUUID())
		await assert.rejects(
			client.startRcloneDaemon({
				binaryPath: join(dir, "rclone"),
				binarySha256: undefined,
				stateDir: join(dir, "state"),
				cacheDir: join(dir, "cache"),
				logFile: undefined,
				userAgent: "filen-sdk-rs-smoke",
				vfs: { cacheMaxSize: 1n << 30n, cacheMinFreeSpace: 1n << 30n, cacheMaxAgeSecs: 3600n, dirCacheTimeSecs: 15 }
			}),
			e => {
				assert.deepEqual(assertThrownLikeUbrn(e).rcloneRefusal(), { tag: RcloneRefusal_Tags.BinaryMissing })
				return true
			}
		)
	})

	test("a binary checksum that is not 64 hex digits rejects as a conversion error", async () => {
		const client = synthClient(randomUUID())
		await assert.rejects(
			client.startRcloneDaemon({
				binaryPath: join(dir, "rclone"),
				binarySha256: "not-hex",
				stateDir: join(dir, "state"),
				cacheDir: join(dir, "cache"),
				logFile: undefined,
				userAgent: "filen-sdk-rs-smoke",
				vfs: { cacheMaxSize: 0n, cacheMinFreeSpace: 0n, cacheMaxAgeSecs: 0n, dirCacheTimeSecs: 0 }
			}),
			e => {
				const inner = assertThrownLikeUbrn(e)
				assert.equal(inner.kind(), ErrorKind.Conversion)
				assert.equal(inner.rcloneRefusal(), undefined)
				return true
			}
		)
	})
})

// A real rclone (FILEN_TEST_RCLONE, as rclone_daemon_tests take it) under a client whose keys no
// server knows: starting, reading stats and stopping never reach the account, as rclone's Filen
// remote logs in only once a mount or serve first reads it.
const rclone = process.env.FILEN_TEST_RCLONE

const daemonOptions = dir => ({
	binaryPath: rclone,
	binarySha256: undefined,
	stateDir: join(dir, "state"),
	cacheDir: join(dir, "cache"),
	logFile: join(dir, "rclone.log"),
	userAgent: "filen-sdk-rs-smoke",
	vfs: { cacheMaxSize: 1n << 30n, cacheMinFreeSpace: 1n << 30n, cacheMaxAgeSecs: 3600n, dirCacheTimeSecs: 15 }
})

/** Whether a process with `pid` runs. */
function isRunning(pid) {
	try {
		process.kill(pid, 0)
		return true
	} catch (e) {
		return e.code === "EPERM"
	}
}

describe("rclone daemon, offline", { skip: rclone ? false : "FILEN_TEST_RCLONE is unset" }, () => {
	let dir

	before(() => {
		dir = tempDir("filen-sdk-rs-rclone-daemon-")
	})

	after(() => removeDir(dir))

	test("a daemon starts, refuses a serve without credentials, and stops", async () => {
		const daemon = await synthClient(randomUUID()).startRcloneDaemon(daemonOptions(join(dir, "stopped")))
		assert.ok(daemon instanceof JsRcloneDaemon)
		const pid = daemon.pid()
		assert.ok(isRunning(pid))
		const stats = await daemon.stats()
		assert.deepEqual(
			[stats.uploadsInProgress, stats.uploadsQueued, stats.transfers, stats.mounts, stats.serves],
			[0, 0, [], [], []]
		)
		assert.ok((await daemon.serveTypes()).includes(ServeProtocol.Webdav))
		await assert.rejects(
			daemon.startServe({
				protocol: ServeProtocol.Webdav,
				host: "127.0.0.1",
				port: 1900,
				auth: { tag: sdk.ServeAuth_Tags.Unauthenticated },
				tls: undefined,
				readOnly: false
			}),
			e => {
				assert.deepEqual(assertThrownLikeUbrn(e).rcloneRefusal(), { tag: RcloneRefusal_Tags.AuthRequired })
				return true
			}
		)

		assert.deepEqual(await daemon.stop({ drainTimeoutMs: undefined, graceMs: 10_000 }), { pendingUploads: 0, forced: false })
		assert.deepEqual(await daemon.exited(), { tag: sdk.RcloneExit_Tags.Stopped })
		assert.equal(isRunning(pid), false)
	})

	test("a daemon left running stops with its process", () => {
		const script = `
			import { generateKeyPairSync } from "node:crypto"
			import { join } from "node:path"
			const { UnauthJsClient } = await import(${JSON.stringify(addon)})
			${synthStringified.toString()}
			const rclone = process.env.FILEN_TEST_RCLONE
			const dir = process.argv.at(-1)
			const daemonOptions = ${daemonOptions.toString()}
			const client = UnauthJsClient.fromConfig({}).fromStringified(synthStringified(crypto.randomUUID()))
			const daemon = await client.startRcloneDaemon(daemonOptions(dir))
			console.log(daemon.pid())
		`
		const run = spawnSync(process.execPath, ["--input-type=module", "-e", script, join(dir, "left")], {
			encoding: "utf8",
			timeout: 30_000
		})
		assert.equal(run.error, undefined, "the process did not exit within 30 s")
		assert.equal(run.status, 0, run.stderr)
		const pid = Number(run.stdout.trim())
		assert.ok(pid > 0, run.stdout)
		assert.equal(isRunning(pid), false)
	})
})

describe("sync engine, offline", () => {
	const listener = { onEvents() {}, onWatchStatus() {} }
	const rootUuid = randomUUID()
	const remoteRoot = randomUUID()
	let dir, local, dbPath, client, engine, attached, id

	before(() => {
		dir = tempDir("filen-sdk-rs-sync-")
		local = join(dir, "local")
		dbPath = join(dir, "baseline.db")
		// No cache, as on the Rust tests' offline client: `addPair` then degrades the pair's remote
		// notifications on the spot, where a configured cache would validate the remote root over the
		// network first.
		client = synthClient(rootUuid)
	})

	after(() => removeDir(dir))

	test("openSyncEngine opens an engine with nothing in it", async () => {
		engine = await client.openSyncEngine(dbPath, listener)
		assert.ok(engine instanceof JsSyncEngine)
		assert.deepEqual(await engine.listPairs(), [])
		assert.deepEqual(await engine.recentEvents(), [])
		assert.equal(await engine.userIgnore(), "")
	})

	test("addPair stores the pair with its defaults", async () => {
		mkdirSync(local)
		id = await engine.addPair(local, remoteRoot, SyncMode.TwoWay)
		assert.equal(typeof id, "bigint")
		const pairs = await engine.listPairs()
		assert.equal(pairs.length, 1)
		const [pair] = pairs
		assert.equal(pair.id, id)
		assert.equal(canon(pair.localRoot), canon(realpathSync.native(local)))
		assert.equal(pair.remoteRoot, remoteRoot)
		assert.equal(pair.mode, SyncMode.TwoWay)
		assert.deepEqual(pair.deleteGuard, { tag: DeleteGuardState_Tags.Limited, inner: { floor: 10n, ratio: 0.5 } })
		assert.equal(pair.paused, false)
		assert.equal(pair.activity, SyncPairActivity.Idle)
		// napi leaves an absent Option's key out; read the value, as ubrn's caller would.
		for (const key of [
			"watchStatus",
			"passInFlight",
			"lastPassCompletedAtMs",
			"lastPassError",
			"lastPassErrorKind",
			"pausedAtMs",
			"pauseOptions"
		]) {
			assert.equal(pair[key], undefined, key)
		}
	})

	test("the delete guard reads back as set", async () => {
		await engine.setDeleteGuard(id, 3n, 0.25)
		assert.deepEqual((await engine.listPairs())[0].deleteGuard, {
			tag: DeleteGuardState_Tags.Limited,
			inner: { floor: 3n, ratio: 0.25 }
		})
		await engine.setDeleteGuardUnlimited(id)
		const guard = (await engine.listPairs())[0].deleteGuard
		assert.deepEqual(guard, { tag: DeleteGuardState_Tags.Unlimited })
		assert.equal("inner" in guard, false)
	})

	test("pause, cancel and resume read back", async () => {
		const pair = async () => (await engine.listPairs())[0]
		await engine.pausePair(id, { mode: PauseMode.Suspend, cancelAfterMs: 60_000n })
		let paused = await pair()
		assert.equal(paused.paused, true)
		assert.equal(typeof paused.pausedAtMs, "bigint")
		assert.deepEqual(paused.pauseOptions, { mode: PauseMode.Suspend, cancelAfterMs: 60_000n })
		await engine.cancelPausedActions(id)
		assert.deepEqual((await pair()).pauseOptions, { mode: PauseMode.Cancel, cancelAfterMs: 60_000n })
		await engine.resumePair(id)
		let resumed = await pair()
		assert.equal(resumed.paused, false)
		assert.equal(resumed.pausedAtMs, undefined)
		assert.equal(resumed.pauseOptions, undefined)

		await engine.pausePair(id, undefined)
		paused = await pair()
		assert.deepEqual(paused.pauseOptions, { mode: PauseMode.Suspend, cancelAfterMs: 300_000n })
		await engine.resumePair(id)
		resumed = await pair()
		assert.equal(resumed.paused, false)
		assert.equal(resumed.pauseOptions, undefined)
	})

	test("conflicts and the last report start empty", async () => {
		assert.deepEqual(await engine.listConflicts(id, 0n, 0n), { conflicts: [], total: 0n })
		assert.equal(await engine.lastReport(id), undefined)
		// The wrapper reads its params inside the promise, so an out-of-range u64 rejects.
		await assert.rejects(engine.listConflicts(id, -1n, 0n), e => {
			assert.equal(assertThrownLikeUbrn(e).kind(), ErrorKind.Conversion)
			return true
		})
	})

	test("importLegacyState reads a filen-sync state directory", async () => {
		const state = join(dir, "filen-sync-state")
		mkdirSync(state)
		assert.deepEqual(await engine.importLegacyState(id, state), { imported: 0n, skipped: 0n })
		writeFileSync(
			join(state, "previousLocalTree"),
			`${JSON.stringify({ prop: "/a.txt", data: { type: "file", size: 5, lastModified: 1700000000123 } })}\nnot json\n`
		)
		writeFileSync(
			join(state, "previousRemoteTree"),
			`${JSON.stringify({ prop: "/a.txt", data: { type: "file", uuid: randomUUID(), size: 5, lastModified: 1700000000000 } })}\n`
		)
		assert.deepEqual(await engine.importLegacyState(id, state), { imported: 1n, skipped: 1n })
		await assert.rejects(engine.importLegacyState(id, join(dir, "missing-state")), e => {
			assert.equal(assertThrownLikeUbrn(e).kind(), ErrorKind.Io)
			return true
		})
	})

	test("a second open of the same baseline by the same account attaches", async () => {
		const sameAccount = synthClient(rootUuid)
		attached = await sameAccount.openSyncEngine(dbPath, listener)
		assert.ok(attached instanceof JsSyncEngine)
		assert.deepEqual(
			(await attached.listPairs()).map(pair => pair.id),
			[id]
		)
	})

	test("another account's open of the same baseline is refused", async () => {
		const otherAccount = synthClient(randomUUID())
		await assert.rejects(otherAccount.openSyncEngine(dbPath, listener), e => {
			const inner = assertThrownLikeUbrn(e)
			assert.equal(inner.kind(), ErrorKind.Io)
			assert.deepEqual(inner.syncRefusal(), { tag: SyncRefusal_Tags.BaselineInUse, inner: { path: dbPath } })
			return true
		})
	})

	test("a listener without onWatchStatus throws napi's own error", () => {
		assert.throws(
			() => client.openSyncEngine(join(dir, "never-opened.db"), { onEvents() {} }),
			e => {
				assert.ok(e instanceof Error)
				assert.equal(e.code, "InvalidArg")
				assert.equal(FilenSdkError.hasInner(e), false)
				return true
			}
		)
	})

	test("an open with an already aborted signal rejects and leaves the baseline free", async () => {
		const abortedPath = join(dir, "aborted.db")
		await assert.rejects(client.openSyncEngine(abortedPath, listener, { signal: AbortSignal.abort() }), e => {
			assert.ok(e instanceof Error)
			assert.equal(e.name, "AbortError")
			assert.equal(e.message, "A Rust future was aborted")
			assert.equal(FilenSdkError.hasInner(e), false)
			return true
		})
		const reopened = await client.openSyncEngine(abortedPath, listener)
		await reopened.close()
	})

	test("an open in a missing directory rejects with a FilenSdkError", async () => {
		await assert.rejects(client.openSyncEngine(join(dir, "missing", "baseline.db"), listener), e => {
			assert.equal(assertThrownLikeUbrn(e).kind(), ErrorKind.Io)
			return true
		})
	})

	test("an open of a baseline at another schema version is refused and leaves it as it was", async () => {
		const userVersion = path => {
			const db = new DatabaseSync(path, { readOnly: true })
			try {
				return db.prepare("PRAGMA user_version").get().user_version
			} finally {
				db.close()
			}
		}
		// The version this build stamps, read off the baseline the suite's engine created.
		const expected = userVersion(dbPath)
		const foreignPath = join(dir, "foreign.db")
		const foreign = new DatabaseSync(foreignPath)
		foreign.exec("CREATE TABLE something (id INTEGER PRIMARY KEY); PRAGMA user_version = 1000;")
		foreign.close()

		await assert.rejects(client.openSyncEngine(foreignPath, listener), e => {
			const inner = assertThrownLikeUbrn(e)
			assert.equal(inner.kind(), ErrorKind.InvalidState)
			assert.deepEqual(inner.syncRefusal(), {
				tag: SyncRefusal_Tags.BaselineSchemaMismatch,
				inner: { path: foreignPath, found: 1000n, expected: BigInt(expected) }
			})
			return true
		})
		assert.equal(userVersion(foreignPath), 1000)
	})

	test("close is idempotent and ends every handle", async () => {
		await engine.close()
		await engine.close()
		for (const handle of [engine, attached]) {
			await assert.rejects(handle.listPairs(), e => {
				const inner = assertThrownLikeUbrn(e)
				assert.equal(inner.kind(), ErrorKind.InvalidState)
				assert.match(inner.toDebugString(), /the sync engine is closed/)
				return true
			})
		}
	})
})

describe("a sync engine closes with its Node environment", () => {
	const listener = { onEvents() {}, onWatchStatus() {} }
	let dir

	before(() => {
		dir = tempDir("filen-sdk-rs-sync-env-")
	})

	after(() => removeDir(dir))

	// The worker's JS handles are finalized as its environment goes, and dropping an engine lets
	// its baseline go too, but in the background, after stopping its watches: without the close the
	// teardown waits for, the baseline is usually still held right after the worker has exited.
	// Usually: the first worker of a process often wins that race, hence three rounds.
	test("one a worker leaves watching is another environment's, and free once the worker has exited", { timeout: 60_000 }, async () => {
		for (const round of [1, 2, 3]) {
			const dbPath = join(dir, `baseline-${round}.db`)
			const rootUuid = randomUUID()
			const worker = watchInWorker(dbPath, rootUuid)
			assert.deepEqual(await once(worker, "message"), ["watching"])

			// The same account, but not the environment that opened it: refused, not attached.
			await assert.rejects(synthClient(rootUuid).openSyncEngine(dbPath, listener), e => {
				assert.deepEqual(assertThrownLikeUbrn(e).syncRefusal(), { tag: SyncRefusal_Tags.BaselineInUse, inner: { path: dbPath } })
				return true
			})
			// Another account, so its open succeeds only once the worker's engine has let the
			// baseline go. Made first: its key generation would give a background release time.
			const otherAccount = synthClient(randomUUID())
			await worker.terminate()
			const engine = await otherAccount.openSyncEngine(dbPath, listener)
			await engine.close()
		}
	})
})

describe("a process with a sync engine exits by itself", () => {
	// One failed open first, so a failed open's dropped listener is shown not to hold the process.
	const script = `
		import { generateKeyPairSync } from "node:crypto"
		import { join } from "node:path"
		const { UnauthJsClient } = await import(${JSON.stringify(addon)})
		${synthStringified.toString()}
		const [dir, variant] = process.argv.slice(-2)
		const client = UnauthJsClient.fromConfig({}).fromStringified(synthStringified(crypto.randomUUID()))
		const listener = { onEvents() {}, onWatchStatus() {} }
		await client.openSyncEngine(join(dir, "missing", "baseline.db"), listener).catch(() => {})
		const engine = await client.openSyncEngine(join(dir, "baseline.db"), listener)
		if (variant === "closed") await engine.close()
	`

	for (const variant of ["unclosed", "closed"]) {
		test(variant, () => {
			const dir = tempDir("filen-sdk-rs-sync-exit-")
			try {
				const run = spawnSync(process.execPath, ["--input-type=module", "-e", script, dir, variant], {
					encoding: "utf8",
					timeout: 30_000
				})
				assert.equal(run.error, undefined, "the process did not exit within 30 s")
				assert.equal(run.status, 0, run.stderr)
			} finally {
				removeDir(dir)
			}
		})
	}
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
