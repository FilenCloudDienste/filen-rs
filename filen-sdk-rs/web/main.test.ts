import init, {
	initThreadPool,
	Client,
	type Dir,
	type File,
	PauseSignal,
	FilenSdkError,
	ListenerHandle,
	type SocketEvent,
	type FileMeta,
	type DecryptedFileMeta,
	type DecryptedDirMeta,
	type DirMeta,
	UnauthClient,
	parseName,
	encodeName,
	decodeName,
	EntryNameErrorJS,
	type AnyLinkedDirWithContext,
	type AnyFile,
	type CacheStatusMessage,
	type CacheSearchSnapshot,
	type MakeThumbnailInMemoryResult,
	type InMemoryThumbnail,
	type EmbeddedPreviewResult,
	type CopyUpdate,
	type AnyItemWithContext,
	type ArchiveEntry,
	type CompressFormat,
	type CompressUpdate,
	type ExtractedTopLevelItem,
	type ExtractRetry,
	type ExtractUpdate,
	type ListUpdate,
	archiveDefaultName,
	archiveEncoderMemory,
	archiveExtension,
	archiveFormatLevels,
	archiveFormatOfName,
	archiveMaxLevel
} from "./sdk-rs.js"
import { expect, beforeAll, test, afterAll, afterEach, vi } from "vitest"
import { ZipReader, Uint8ArrayWriter, type Entry } from "@zip.js/zip.js"

console.log("Initializing WASM...")
const wasm = await init()
// wasm linear memory only ever grows (1 GiB linker cap, never returned to
// the host), so this is a monotone high-water mark of the whole module.
const heapBytes = () => wasm.memory.buffer.byteLength
const threads = Math.max((navigator.hardwareConcurrency || 5) - 1, 1)
console.log(`WASM initialized ${threads} threads`)
const now = Date.now()
await initThreadPool(threads)
console.log(`WASM initialized ${threads} in ${Date.now() - now}ms`)

/// `makeThumbnailInMemory` now answers with a verdict, not `undefined`: a
/// caller can tell "we do not decode this" from "too expensive here" from
/// "these bytes are broken". Tests that expect a picture assert the happy
/// variant and narrow to it.
function expectThumbnail(result: MakeThumbnailInMemoryResult): InMemoryThumbnail {
	expect(result.type).toBe("thumbnail")
	if (result.type !== "thumbnail") {
		throw new Error(`expected a thumbnail, got ${JSON.stringify(result)}`)
	}
	return result.thumbnail
}

/// An explicit per-test timeout, stretched by the same `VITE_TEST_TIMEOUT_MULT` that
/// vitest.config.ts applies to the 3-minute default. Bounds that measure the SDK itself
/// — the 30 s parked-worker check — are deliberately left unscaled.
const TIMEOUT_MULT = Number(import.meta.env.VITE_TEST_TIMEOUT_MULT) || 1

function cap(ms: number): number {
	return ms * TIMEOUT_MULT
}

let state: Client
let shareClient: Client
let testDir: Dir
const allEvents: SocketEvent[] = []
const listenerHandles: ListenerHandle[] = []
// let _shareTestDir: Dir
const listenerErrors: Error[] = []

// Random-suffixed like the native suites' `rs-<random>` dirs. A fixed name let any
// other invocation of this suite — a local wasm-test.sh, a concurrent run — find and
// permanently delete the live run's parent dir mid-run, which once took out the whole
// tail of a nightly. Stale leftovers are swept by age in beforeAll instead.
const testDirName = `wasm-test-dir-${Array.from(crypto.getRandomValues(new Uint8Array(6)), b =>
	b.toString(16).padStart(2, "0")
).join("")}`

/// The suite-wide capture: correctness checks on every event, plus a black box for the
/// shared account — if something server-side trashes, deletes or moves the suite's
/// parent dir mid-run, the wire event is the only witness that can name the actor's
/// change before unrelated tests start failing on cannot_create_in_this_folder.
function suiteListener(event: SocketEvent) {
	if (!assertNoMaps(event)) {
		listenerErrors.push(new Error("Socket event contained a Map", { cause: event }))
	}
	allEvents.push(event)
	if (testDir && event.type === "drive") {
		const inner = event.inner
		const itemUuid = "uuid" in inner ? inner.uuid : "dir" in inner ? inner.dir.uuid : undefined
		const hitsTestDir =
			itemUuid === testDir.uuid &&
			(inner.type === "folderMove" || inner.type === "folderTrash" || inner.type === "folderDeletedPermanent")
		if (hitsTestDir || inner.type === "trashEmpty" || inner.type === "deleteAll") {
			console.error(`suite fixture at risk, ${inner.type} event:`, JSON.stringify(event, jsonBigIntReplacer))
		}
	}
}

function assertNoMaps(value: unknown): boolean {
	if (value instanceof Map) {
		return false
	}
	if (value && typeof value === "object") {
		for (const key in value as object) {
			if (!assertNoMaps((value as Record<string, unknown>)[key])) {
				return false
			}
		}
	}
	return true
}

const unauthClient = UnauthClient.from_config({})

beforeAll(async () => {
	await Promise.all([
		(async () => {
			if (!import.meta.env.VITE_TEST_EMAIL) {
				throw new Error("VITE_TEST_EMAIL environment variable is not set")
			}
			if (!import.meta.env.VITE_TEST_PASSWORD) {
				throw new Error("VITE_TEST_PASSWORD environment variable is not set")
			}
			state = await unauthClient.login({
				email: import.meta.env.VITE_TEST_EMAIL,
				password: import.meta.env.VITE_TEST_PASSWORD
			})

			console.log("logged in, setting up socket listener")
			listenerHandles.push(await state.addEventListener(suiteListener, null))

			// Sweep stale fixtures by age, never by exact name: a leftover older than the
			// cutoff is a dead run's debris, while anything younger may be a run that is
			// live right now — deleting it would recreate the mid-run cascade the random
			// suffix exists to prevent. The prefix match also picks up the fixed-name
			// "wasm-test-dir" that older revisions of this suite left behind.
			const listing = await state.listDir(state.root())
			const cutoff = BigInt(Date.now() - 6 * 60 * 60 * 1000)
			await Promise.all(
				listing.dirs
					.filter(dir => {
						const meta = getDirMeta(dir.meta)
						return (
							meta !== null &&
							/^wasm-test-dir(-|$)/.test(meta.name) &&
							meta.created !== undefined &&
							meta.created < cutoff
						)
					})
					.map(dir =>
						// Best-effort, like test-utils' cleanup: one transient failure on a
						// stale leftover must not reject beforeAll and take the whole file
						// down — a worse blast radius than the leak it was sweeping.
						state.deleteDirPermanently(dir).catch(e => console.warn("stale fixture sweep failed", dir.uuid, e))
					)
			)
			testDir = await state.createDir(state.root(), testDirName)
		})(),
		(async () => {
			if (!import.meta.env.VITE_TEST_SHARE_EMAIL) {
				throw new Error("VITE_TEST_SHARE_EMAIL environment variable is not set")
			}
			if (!import.meta.env.VITE_TEST_SHARE_PASSWORD) {
				throw new Error("VITE_TEST_SHARE_PASSWORD environment variable is not set")
			}
			shareClient = await unauthClient.login({
				email: import.meta.env.VITE_TEST_SHARE_EMAIL,
				password: import.meta.env.VITE_TEST_SHARE_PASSWORD
			})
		})()
	])
})

afterEach(() => {
	if (listenerErrors.length > 0) {
		const errors = [...listenerErrors]
		listenerErrors.length = 0
		console.error("Socket listener errors detected:", errors[0].cause)
		throw errors
	}
})

function getFileMeta(meta: FileMeta): DecryptedFileMeta | null {
	if (meta.type === "decoded") {
		return meta.data
	} else {
		return null
	}
}

function getDirMeta(meta: DirMeta): DecryptedDirMeta | null {
	if (meta.type === "decoded") {
		return meta.data
	} else {
		return null
	}
}

/// Draws a `width`x`height` gradient on an OffscreenCanvas and encodes it, so
/// the large fixtures below cost no bytes in the repo.
async function generateImage(width: number, height: number, type: string): Promise<Uint8Array<ArrayBuffer>> {
	const canvas = new OffscreenCanvas(width, height)
	const ctx = canvas.getContext("2d")
	if (!ctx) {
		throw new Error("OffscreenCanvas 2d context unavailable")
	}
	const gradient = ctx.createLinearGradient(0, 0, width, height)
	gradient.addColorStop(0, "#ff0055")
	gradient.addColorStop(0.5, "#00ccff")
	gradient.addColorStop(1, "#ffee00")
	ctx.fillStyle = gradient
	ctx.fillRect(0, 0, width, height)
	// Photographic-ish high-frequency detail, so a JPEG of this does not
	// compress away to nothing and the decode does real work.
	for (let i = 0; i < 4000; i++) {
		ctx.fillStyle = `rgb(${(i * 37) % 256},${(i * 91) % 256},${(i * 53) % 256})`
		ctx.fillRect((i * 131) % width, (i * 197) % height, 40, 40)
	}
	const blob = await canvas.convertToBlob({ type, quality: 0.9 })
	if (blob.type !== type) {
		throw new Error(`browser encoded ${type} as ${blob.type}`)
	}
	return new Uint8Array(await blob.arrayBuffer())
}

// FIRST in the file on purpose: wasm linear memory is a monotone high-water
// mark, so any earlier test's peak would mask the growth this one measures.
test("thumbnail decode stays inside a bounded memory budget", async () => {
	// A 12 MP PNG, not a JPEG: a JPEG is IDCT-scaled by its own decoder, so
	// even the whole-frame path it replaced would never have materialised the
	// full frame — the old fixture could not fail this test. A PNG has no such
	// escape: decoded whole-frame it is 48 MB of RGBA (the browser's canvas
	// always writes an alpha channel), plus a second copy for the resize.
	// Through microthumb its rows stream into a canvas sized by the REQUEST.
	const bytes = await generateImage(4000, 3000, "image/png")
	// Sampled before the upload, so the upload's own buffers cannot pre-grow
	// the mark and hide what the decode then spends inside it.
	const before = heapBytes()
	const file = await state.uploadFile(bytes, { parent: testDir, name: "memory-12mp.png" })
	expect(file.canMakeThumbnail).toBe(true)

	const thumb = expectThumbnail(await state.makeThumbnailInMemory({ file, maxHeight: 256, maxWidth: 256 }))
	const after = heapBytes()
	console.log(
		`12 MP png thumbnail: source ${bytes.length} B, heap ${before} -> ${after} (+${after - before} bytes)`
	)

	const bitmap = await createImageBitmap(new Blob([thumb.webpData], { type: "image/webp" }))
	expect(bitmap.width).toBeLessThanOrEqual(256)
	expect(bitmap.height).toBeLessThanOrEqual(256)
	bitmap.close()

	// Upload buffers, the decode worker's own thread state and a ~7 MiB
	// accumulator: the bounded pipeline lands around 20 MiB. The source bytes
	// are NOT in here any more — the decode streams them a chunk at a time
	// straight off the network. The 48 MB frame alone would clear
	// this bound, which is the regression the test exists to catch. The
	// remaining headroom is the upload's, not the decode's — the mark is taken
	// before `uploadFile` so that growth is inside the delta. A change that
	// buffers uploads differently moves this number without the thumbnail
	// path regressing; re-measure before treating it as a decode leak.
	expect(after - before).toBeLessThan(40 * 1024 * 1024)
	// And memory is never returned, so a later reading can only be >=.
	expect(heapBytes()).toBeGreaterThanOrEqual(after)
})

test("login", async () => {
	expect(state).toBeDefined()
	expect(state.root().uuid).toBeDefined()
})

test("account info", async () => {
	const info = await state.getUserInfo()
	expect(info.email).toBe(import.meta.env.VITE_TEST_EMAIL)
})

test("serialization", async () => {
	const serializedState = await state.toStringified()
	expect(serializedState.rootUuid).toEqual(state.root().uuid)
	const newState = unauthClient.fromStringified(serializedState)
	expect(newState.root().uuid).toEqual(state.root().uuid)
})

test("list root directory", async () => {
	const root = state.root()
	expect(root).toBeDefined()
	expect(root.uuid).toBeDefined()
	const resp = await state.listDir(root)
	expect(resp).toBeDefined()
	expect(resp.dirs).toBeInstanceOf(Array)
	expect(resp.files).toBeInstanceOf(Array)
})

test("Directory", async () => {
	const before = new Date().getTime()
	let dir = await state.createDir(testDir, "test-dir")
	const { dirs, files } = await state.listDir(dir)
	expect(dirs.length).toBe(0)
	expect(files.length).toBe(0)

	const after = new Date().getTime()
	expect(dir).toBeDefined()
	expect(dir.uuid).toBeDefined()
	expect(dir.parent).toBe(testDir.uuid)
	const meta = getDirMeta(dir.meta)
	expect(meta?.name).toBe("test-dir")
	expect(meta?.created).toBeGreaterThanOrEqual(before)
	expect(meta?.created).toBeLessThanOrEqual(after)
	dir = await state.trashDir(dir)
	expect(dir.parent).toBe("trash")
	await state.deleteDirPermanently(dir)
})

test("File", async () => {
	const created = BigInt(new Date().getTime())
	const before = BigInt(new Date().getTime())
	let file = await state.uploadFile(new TextEncoder().encode("test-file.txt"), {
		parent: testDir,
		name: "test-file.txt",
		created: created
	})
	const after = new Date().getTime()
	expect(file).toBeDefined()
	expect(file.uuid).toBeDefined()
	expect(file.parent).toBe(testDir.uuid)
	const meta = getFileMeta(file.meta)
	expect(meta?.name).toBe("test-file.txt")
	expect(meta?.created).toStrictEqual(created)
	expect(meta?.modified).toBeGreaterThanOrEqual(before)
	expect(meta?.modified).toBeLessThanOrEqual(after)
	expect(file.size).toBe(BigInt("test-file.txt".length))
	const data = await state.downloadFile(file)
	expect(new TextDecoder().decode(data)).toBe("test-file.txt")
	file = await state.trashFile(file)
	expect(file.parent).toBe("trash")
	await state.deleteFilePermanently(file)
})

test("File Streams", async () => {
	const data = "test file data"
	const blob = new Blob([data])

	// Upload test
	let progress = 0n
	const remoteFile = await state.uploadFileFromReader({
		parent: testDir,
		name: "stream-file.txt",
		reader: blob.stream(),
		progress: (bytes: bigint) => {
			progress = bytes
		},
		knownSize: data.length
	})

	expect(progress).toBe(BigInt(data.length))

	// Helper to collect stream into bytes
	const collectBytes = async (downloadFn: (writer: WritableStream<Uint8Array>) => Promise<void>): Promise<Uint8Array> => {
		const chunks: Uint8Array[] = []
		await downloadFn(
			new WritableStream<Uint8Array>({
				write(chunk: Uint8Array) {
					chunks.push(chunk)
				}
			})
		)
		// Manually concatenate chunks to avoid type issues
		const totalLength = chunks.reduce((sum, chunk) => sum + chunk.length, 0)
		const result = new Uint8Array(totalLength)
		let offset = 0
		for (const chunk of chunks) {
			result.set(chunk, offset)
			offset += chunk.length
		}
		return result
	}

	// Full download test
	let downloadProgress = 0n
	const downloadedBytes = await collectBytes((writer: WritableStream<Uint8Array>) =>
		state.downloadFileToWriter({
			file: remoteFile,
			writer,
			progress: (bytes: bigint) => {
				downloadProgress = bytes
			}
		})
	)

	expect(downloadProgress).toBe(BigInt(data.length))
	expect([...downloadedBytes]).toEqual([...new TextEncoder().encode(data)])

	// Partial download test
	const partialBytes = await collectBytes((writer: WritableStream<Uint8Array>) =>
		state.downloadFileToWriter({
			file: remoteFile,
			writer,
			start: BigInt(5),
			end: BigInt(9)
		})
	)

	expect([...partialBytes]).toEqual([...new TextEncoder().encode("file")])
})

test("abort", async () => {
	const abortController = new AbortController()
	const fileAPromise = state.uploadFile(new TextEncoder().encode("file a"), {
		name: "abort a.txt",
		parent: testDir,
		managedFuture: {
			abortSignal: abortController.signal
		}
	})

	const fileBPromise = state.uploadFile(new TextEncoder().encode("file b"), {
		name: "abort b.txt",
		parent: testDir
	})

	const abortControllerDelayed = new AbortController()

	const fileCPromise = state.uploadFile(new TextEncoder().encode("file c"), {
		name: "abort c.txt",
		parent: testDir,
		managedFuture: {
			abortSignal: abortControllerDelayed.signal
		}
	})
	setTimeout(() => {
		abortControllerDelayed.abort()
	}, 20)

	abortController.abort()

	try {
		await fileAPromise
	} catch (e) {
		expect(e).toBeInstanceOf(FilenSdkError)
		expect((e as FilenSdkError).kind).toBe("Cancelled")
	}
	try {
		await fileCPromise
	} catch (e) {
		expect(e).toBeInstanceOf(FilenSdkError)
		expect((e as FilenSdkError).kind).toBe("Cancelled")
	}
	const fileB = await fileBPromise
	const { files } = await state.listDir(testDir)

	expect(files).toContainEqual(fileB)
	for (const file of files) {
		const meta = getFileMeta(file.meta)
		expect(meta?.name).not.toBe("abort a.txt")
		expect(meta?.name).not.toBe("abort c.txt")
	}
})

test("aborted download never closes the stream with truncated data", async () => {
	const size = 4 * 1024 * 1024
	const remoteFile = await state.uploadFile(new Uint8Array(size), {
		name: "abort stream.bin",
		parent: testDir
	})

	const abortController = new AbortController()
	let closed = false
	let aborted = false
	let received = 0
	const writer = new WritableStream<Uint8Array>({
		write(chunk: Uint8Array) {
			received += chunk.length
			// abort on the first flushed chunk, then stall the sink: the download
			// promise only settles once the buffered write task drains its frames
			// through the sink, so the stall keeps it pending until the abort
			// signal crosses to the commander worker and cancels it
			abortController.abort()
			return new Promise(resolve => setTimeout(resolve, 100))
		},
		close() {
			closed = true
		},
		abort() {
			aborted = true
		}
	})

	let error: unknown
	try {
		await state.downloadFileToWriter({
			file: remoteFile,
			writer,
			managedFuture: {
				abortSignal: abortController.signal
			}
		})
	} catch (e) {
		error = e
	}
	expect(error).toBeInstanceOf(FilenSdkError)
	expect((error as FilenSdkError).kind).toBe("Cancelled")

	// the promise settles as soon as the abort fires, while the background write
	// task still drains its buffered frames (each paying the sink stall) before
	// sealing the stream — give the terminal state ample time
	await vi.waitFor(
		() => {
			expect(aborted || closed).toBe(true)
		},
		{ timeout: 15_000 }
	)

	// The invariant under test: a producer that died mid-stream must abort()
	// the stream. If the abort instead raced past a download that had already
	// fully completed (its Done sentinel sent), a clean close is legitimate —
	// but a clean close with TRUNCATED data is exactly the
	// truncated-file-reported-as-complete bug this pins.
	if (closed) {
		expect(received).toBe(size)
		expect(aborted).toBe(false)
	} else {
		expect(aborted).toBe(true)
	}
})

test("pause", async () => {
	const pauseSignal = new PauseSignal()
	let fileAPromiseResolved = false
	const fileAPromise = state.uploadFile(new TextEncoder().encode("file a"), {
		name: "pause a.txt",
		parent: testDir,
		managedFuture: {
			pauseSignal: pauseSignal
		}
	})
	fileAPromise.then(() => {
		fileAPromiseResolved = true
	})
	console.log("Pausing")
	pauseSignal.pause()
	console.log("Paused", pauseSignal.isPaused())

	let fileBPromiseResolved = false
	const fileBPromise = state.uploadFile(new TextEncoder().encode("file b"), {
		name: "pause b.txt",
		parent: testDir,
		managedFuture: {
			pauseSignal: pauseSignal
		}
	})
	fileBPromise.then(() => {
		fileBPromiseResolved = true
	})

	const fileCPromise = state.uploadFile(new TextEncoder().encode("file c"), {
		name: "pause c.txt",
		parent: testDir
	})

	let fileDPromiseResolved = false
	const fileDPromise = state.uploadFile(new TextEncoder().encode("file d"), {
		name: "pause d.txt",
		parent: testDir
	})
	fileDPromise.then(() => {
		fileDPromiseResolved = true
	})

	console.log("awaiting first file (c)")
	const fileC = await fileCPromise
	console.log("file c done")
	expect(fileC).toBeDefined()
	const metaC = getFileMeta(fileC.meta)
	expect(metaC?.name).toBe("pause c.txt")
	await new Promise(resolve => setTimeout(resolve, 5000))
	expect(fileAPromiseResolved).toBe(false)
	expect(fileBPromiseResolved).toBe(false)
	expect(fileDPromiseResolved).toBe(true)
	pauseSignal.resume()
	console.log("resumed, awaiting a and b")
	const fileA = await fileAPromise
	console.log("file a done")
	expect(fileA).toBeDefined()
	const metaA = getFileMeta(fileA.meta)
	expect(metaA?.name).toBe("pause a.txt")
	console.log("awaiting b")
	await new Promise(resolve => setTimeout(resolve, 5000))
	console.log("checking b")
	expect(fileBPromiseResolved).toBe(true)
	const fileB = await fileBPromise
	expect(fileB).toBeDefined()
	const metaB = getFileMeta(fileB.meta)
	expect(metaB?.name).toBe("pause b.txt")
})

// This test only passes Dir items at the top level; nested files are verified as zip entries.
test("Zip Download", async () => {
	const dirA = await state.createDir(testDir, "zip-a")
	const dirB = await state.createDir(dirA, "b")

	const file1 = await state.uploadFile(new TextEncoder().encode("file 1 content"), {
		parent: dirA,
		name: "file1.txt"
	})
	const file2 = await state.uploadFile(new TextEncoder().encode("file 2 content"), {
		parent: dirB,
		name: "file2.txt"
	})
	const file3 = await state.uploadFile(new TextEncoder().encode("file 3 content"), {
		parent: dirB,
		name: "file3.txt"
	})

	const { readable, writable } = new TransformStream<Uint8Array>()

	let lastBytesWritten = 0n
	let lastTotalBytes = 0n
	let progressCallCount = 0

	// Do not await here: TransformStream has no internal buffer. Awaiting before consuming
	// the readable side would deadlock (the writer blocks when the reader is not draining).
	// Instead save the promise and await it after consuming all zip entries.
	let downloadError: unknown = undefined
	const downloadPromise = state
		.downloadItemsToZip(
			[dirA],
			writable,
			(bytesWritten: bigint, totalBytes: bigint, _itemsProcessed: bigint, _totalItems: bigint) => {
				lastBytesWritten = bytesWritten
				lastTotalBytes = totalBytes
				progressCallCount++
			},
			{}
		)
		.catch((e: unknown) => {
			downloadError = e
		})

	const zipReader = new ZipReader<ReadableStream<Uint8Array>>(readable)

	let zipError: unknown = undefined
	const entries = await zipReader.getEntries().catch((e: unknown) => {
		zipError = e
		return [] as Entry[]
	})
	// Await the download to surface any SDK error before asserting zip results
	await downloadPromise
	if (downloadError !== undefined) {
		throw new Error(`downloadItemsToZip failed: ${downloadError}`)
	}
	if (zipError !== undefined) {
		throw new Error(`ZipReader.getEntries failed: ${zipError}`)
	}
	const map = new Map<string, Entry>()
	for (const entry of entries) {
		map.set(entry.filename, entry)
	}

	const compareFileToEntry = async (entry: Entry, expected: Uint8Array, expectedFile: File) => {
		if (entry.directory) {
			throw new Error("Expected entry to be a FileEntry, but it was a directory")
		}
		// zip.js has bad precision for dates, so we compare in seconds
		const meta = getFileMeta(expectedFile.meta)
		expect(BigInt(entry.creationDate!.getTime())).toEqual(meta?.created)
		expect(entry.lastModDate.getTime() / 1000).toEqual(Math.floor(Number(meta?.modified) / 1000))
		expect(BigInt(entry.uncompressedSize)).toEqual(expectedFile.size)
		const data = await entry.getData(new Uint8ArrayWriter())
		expect(data).toEqual(expected)
	}

	await compareFileToEntry(map.get("zip-a/file1.txt")!, new TextEncoder().encode("file 1 content"), file1)
	await compareFileToEntry(map.get("zip-a/b/file2.txt")!, new TextEncoder().encode("file 2 content"), file2)
	await compareFileToEntry(map.get("zip-a/b/file3.txt")!, new TextEncoder().encode("file 3 content"), file3)

	// verify progress callback fired and final counters are consistent
	expect(progressCallCount).toBeGreaterThan(0)
	expect(lastBytesWritten).toBeGreaterThan(0n)
	expect(lastBytesWritten).toBeLessThanOrEqual(lastTotalBytes)
})

/// The share account as a contact of the main one, whatever state an earlier test left the
/// pair in: already contacts, a request from the main account still pending (the "block"
/// test ends that way), or nothing yet.
async function ensureShareContact() {
	const shareEmail = import.meta.env.VITE_TEST_SHARE_EMAIL!
	const existing = (await state.getContacts()).find(c => c.email === shareEmail)
	if (existing) {
		return existing
	}
	let request = (await shareClient.listIncomingContactRequests()).find(r => r.email === import.meta.env.VITE_TEST_EMAIL)
	if (!request) {
		const requestUuid = await state.sendContactRequest(shareEmail)
		request = (await shareClient.listIncomingContactRequests()).find(r => r.uuid === requestUuid)
		if (!request) {
			throw new Error("Contact request not found")
		}
	}
	await shareClient.acceptContactRequest(request.uuid)
	const contact = (await state.getContacts()).find(c => c.email === shareEmail)
	if (!contact) {
		throw new Error("Contact not listed after accepting the request")
	}
	return contact
}

test("sharing", async () => {
	// Same lock, same order, as the native contact tests — see the shared/linked thumbnail test.
	using _contactLock = await state.acquireLock({ resource: "test:contact" })
	using _shareContactLock = await shareClient.acquireLock({ resource: "test:contact" })
	const dir = await state.createDir(testDir, "share-test-dir")
	const file = await state.uploadFile(new TextEncoder().encode("shared file content"), {
		parent: dir,
		name: "shared-file.txt"
	})

	const contact = await ensureShareContact()
	await state.shareDir(dir, contact, (downloaded: number, total: number | undefined) => {
		console.log(`Shared dir upload progress: ${downloaded}/${total}`)
	})
	const shared = await state.listOutShared(contact)
	const sharedDir = shared.dirs.find(d => d.inner.uuid === dir.uuid)
	expect(sharedDir).toBeDefined()
	expect(sharedDir?.inner?.uuid).toEqual(dir.uuid)

	await shareClient.listInShared()
	const sharedDirs = (await shareClient.listInShared()).dirs
	let sharedDirIn = sharedDirs.find(d => d.inner.uuid === dir.uuid)
	expect(sharedDirIn).toBeDefined()
	sharedDirIn = sharedDirIn!

	const files = (await shareClient.listSharedDir(sharedDirIn, sharedDirIn.sharingRole)).files
	expect(files.find(f => f.uuid === file.uuid)).toBeDefined()

	await state.deleteContact(contact.uuid)
})

test("block", async () => {
	// Same lock, same order, as the native contact tests — see the shared/linked thumbnail test.
	using _contactLock = await state.acquireLock({ resource: "test:contact" })
	using _shareContactLock = await shareClient.acquireLock({ resource: "test:contact" })
	const contacts = await state.getContacts()
	let contact
	for (const c of contacts) {
		if (c.email === import.meta.env.VITE_TEST_SHARE_EMAIL) {
			contact = c
			break
		}
	}
	if (contact) {
		await state.deleteContact(contact.uuid)
		const requests = await state.listOutgoingContactRequests()
		for (const req of requests) {
			console.log("Cancelling existing contact request")
			await state.cancelContactRequest(req.uuid)
		}
	}
	await state.sendContactRequest(import.meta.env.VITE_TEST_SHARE_EMAIL!)
	const requests = await shareClient.listIncomingContactRequests()
	const req = requests.find(r => r.email === import.meta.env.VITE_TEST_EMAIL)
	if (!req) {
		throw new Error("Contact request not found")
	}

	await shareClient.blockContact(req.email)
	const blocked = await shareClient.getBlockedContacts()
	expect(blocked.length).toBe(1)
	expect(blocked[0].email).toBe(import.meta.env.VITE_TEST_EMAIL)

	const requestsAfter = await shareClient.listIncomingContactRequests()
	expect(requestsAfter.length).toBe(requests.length - 1)

	await shareClient.unblockContact(blocked[0].uuid)
	const blockedAfter = await shareClient.getBlockedContacts()
	expect(blockedAfter.length).toBe(0)

	const requestsFinal = await shareClient.listIncomingContactRequests()
	expect(requestsFinal.length).toBe(1)
	expect(requestsFinal[0].email).toBe(import.meta.env.VITE_TEST_EMAIL)
})

test("thumbnail", async () => {
	const imgs = [
		["parrot", "avif"],
		["parrot", "heif"],
		["parrot", "gif"],
		["parrot", "jpg"],
		["parrot", "png"],
		["parrot", "qoi"],
		["parrot", "tiff"],
		["parrot", "webp"]
	]

	const completed: string[] = []

	await Promise.all(
		imgs.map(async ([img, ext]) => {
			const parrotImage = await fetch(`imgs/${img}.${ext}`)
			const file = await state.uploadFile(await parrotImage.bytes(), {
				parent: testDir,
				name: `${img}.${ext}`
			})

			if (!file.canMakeThumbnail) {
				console.warn(`Skipping thumbnail test for unsupported mime type: ${getFileMeta(file.meta)?.mime}`)
				return
			}

			const thumb = expectThumbnail(
				await state.makeThumbnailInMemory({
					file: file,
					maxHeight: 100,
					maxWidth: 100
				})
			)

			// The reported size is what was really encoded, so it must agree
			// with the decoded bitmap below.
			expect(thumb.width).toBeLessThanOrEqual(100)
			expect(thumb.height).toBeLessThanOrEqual(100)

			const blob = new Blob([thumb.webpData], { type: "image/webp" })
			const bitmap = await createImageBitmap(blob)

			expect(bitmap.width).toBe(thumb.width)
			expect(bitmap.height).toBe(thumb.height)

			expect(blob.type).toBe("image/webp")

			// Clean up
			bitmap.close()

			completed.push(ext)
		})
	)

	// avif works here because libheif's AV1 backend is dav1d, which contains no
	// setjmp/longjmp — the thing that kept libaom out of the browser, since on
	// wasm setjmp needs the exception-handling proposal and wasm-bindgen cannot
	// round-trip the tag that leaves behind. See heif-decoder's build_dav1d.
	expect(completed).toContainEqual("avif")
	expect(completed).toContainEqual("gif")
	expect(completed).toContainEqual("heif")
	expect(completed).toContainEqual("jpg")
	expect(completed).toContainEqual("png")
	expect(completed).toContainEqual("tiff")
	expect(completed).toContainEqual("qoi")
	expect(completed).toContainEqual("webp")
})

test("large webp thumbnail", async () => {
	// WebP has no streaming decoder here, so microthumb prices it at a full
	// RGBA frame plus a copy (8 bytes per source pixel) and the budget decides.
	// The committed parrot.webp is 0.67 MP and fits any budget; 3.84 MP does not
	// fit the 12 MiB default, and is exactly what APP_PROCESS_MEM_BUDGET buys.
	const bytes = await generateImage(2400, 1600, "image/webp")
	const file = await state.uploadFile(bytes, { parent: testDir, name: "large.webp" })
	expect(file.canMakeThumbnail).toBe(true)

	const thumb = expectThumbnail(await state.makeThumbnailInMemory({ file, maxHeight: 256, maxWidth: 256 }))

	const bitmap = await createImageBitmap(new Blob([thumb.webpData], { type: "image/webp" }))
	expect(bitmap.width).toBeLessThanOrEqual(256)
	expect(bitmap.height).toBeLessThanOrEqual(256)
	// Aspect preserved (contain, not cover): 3:2 into a 256 box.
	expect(bitmap.width).toBe(256)
	expect(bitmap.height).toBeGreaterThan(150)
	// A generated WebP carries no embedded preview, so this can only have been
	// a real (streamed, bounded) decode.
	expect(thumb.fromEmbeddedPreview).toBe(false)
	bitmap.close()
})

/// The first chunk's FourCC of a WebP: `VP8L` lossless, `VP8 ` simple lossy, `VP8X` extended.
function webpFourcc(webp: Uint8Array): string {
	return new TextDecoder().decode(webp.subarray(12, 16))
}

test("lossy webp thumbnails", async () => {
	// Opaque and photographic, so the lossy encoding is a plain `VP8 ` frame and
	// has real detail to trade away.
	const bytes = await generateImage(1200, 800, "image/jpeg")
	const file = await state.uploadFile(bytes, { parent: testDir, name: "lossy.jpg" })
	expect(file.canMakeThumbnail).toBe(true)

	const lossless = expectThumbnail(await state.makeThumbnailInMemory({ file, maxWidth: 256, maxHeight: 256 }))
	expect(webpFourcc(lossless.webpData)).toBe("VP8L")

	const lossy = expectThumbnail(await state.makeThumbnailInMemory({ file, maxWidth: 256, maxHeight: 256, lossyQuality: 75 }))
	expect(webpFourcc(lossy.webpData)).toBe("VP8 ")
	expect(lossy.webpData.length).toBeLessThanOrEqual(lossless.webpData.length)
	expect([lossy.width, lossy.height]).toEqual([lossless.width, lossless.height])

	const fromStream = expectThumbnail(
		await state.makeThumbnailFromStream({
			reader: new Blob([bytes]).stream(),
			knownSize: bytes.length,
			maxWidth: 256,
			maxHeight: 256,
			lossyQuality: 75
		})
	)
	expect(webpFourcc(fromStream.webpData)).toBe("VP8 ")
	const bitmap = await createImageBitmap(new Blob([fromStream.webpData], { type: "image/webp" }))
	expect([bitmap.width, bitmap.height]).toEqual([fromStream.width, fromStream.height])
	bitmap.close()
})

/// Camera RAW, the case the extension gate exists for: the stored mime is whatever the
/// uploading client's mime table said (RAW is absent from every JS one, and `.CR3` from
/// most), so gated on the mime every RAW on the drive answered "unsupported" while the
/// mobile cache — gated on the extension — thumbnailed them fine. The pipeline never
/// decodes a sensor mosaic; it locates the JPEG the camera embedded and thumbnails that,
/// so the answer must also SAY it came from the embedded preview.
///
/// Pinned CC0 samples from raw.pixls.us, a subset of microthumb's characterisation set
/// (microthumb/tests/raw_fixtures/pins.rs), one per container mechanism: IFD0 strip
/// (CR2), ISO-BMFF box (CR3), SubIFD (NEF), preview IFD (DNG), header pointer (RAF),
/// maker note (ORF), private tag (RW2). They come through the dev server's `/raw-fixtures`
/// cache (vitest.config.ts) — the same files the Rust suite pins — and are verified by
/// length and SHA-256 before anything is uploaded. An absent file is a skip: the library
/// is a volunteer-run host and its outages are not this SDK's; a wrong file is a failure.
///
/// `preview` is what `writeEmbeddedPreview` must hand out for the same file: the
/// embedded JPEG's own dimensions, as microthumb's characterisation measured them
/// (`the_located_preview_is_the_jpeg_it_claims`), or null where the file embeds nothing
/// past the 512 px preview floor — the E-10 carries a 160x120 stamp and no more, which
/// still thumbnails but is not a preview.
const RAW_FIXTURES = [
	{
		name: "2102.CR2",
		path: "2102/nice/Canon%20-%20EOS%2040D%20-%20sRAW2%20%28sRAW%29%20%283%3A2%29.CR2",
		length: 5805950,
		sha256: "ba644e7dd2abe74eca260e67f0206ff113bf0f62e710f8130611e964d6be5bf1",
		preview: { width: 1936, height: 1288 }
	},
	{
		name: "4659.CR3",
		path: "4659/nice/Canon%20-%20EOS%20R6%20-%203%3A2.CR3",
		length: 5273174,
		sha256: "74abb0a113d075ad9887a058082f40dd2a938c4813a08474d82356f11a027778",
		// The full picture from the first track, not the 1620x1080 PRVW box the
		// thumbnail is cut from.
		preview: { width: 3408, height: 2272 }
	},
	{
		name: "7737.NEF",
		path: "7737/nice/Nikon%20-%20Z5_2%20-%208bit%20compressed%20%283%3A2%29.NEF",
		length: 6174720,
		sha256: "196971ea960cfa6f5c0a19cebe515ac118e079c57ceeef850ab3a4995dc7d20f",
		preview: { width: 3984, height: 2656 }
	},
	{
		name: "1033.DNG",
		path: "1033/nice/Adobe%20DNG%20Converter%20-%20Canon%20EOS%205D%20Mark%20III%20-%20Lossy%20JPEG%20compression%2C%20rgb%20%283%3A2%29.DNG",
		length: 2484836,
		sha256: "b22f1e36331f679abb8b13e433b9bbde3988723adf10fee7aa0dda6381016a98",
		preview: { width: 3960, height: 2640 }
	},
	{
		name: "2726.RAF",
		path: "2726/nice/Fujifilm%20-%20FinePix%20S5000%20-%204%3A3.RAF",
		length: 6851240,
		sha256: "dabd5e74521a6980156be9fd4b88d0c37b0fe4d0e0e6f5c12db8cffff1b76297",
		preview: { width: 1280, height: 960 }
	},
	{
		name: "5424.ORF",
		path: "5424/nice/Olympus%20-%20E-10%20-%2016bit%20%284%3A3%29.ORF",
		length: 7614592,
		sha256: "2bfdade72439017a60a47aad1e5bbcb1aca36f2be7a21e94a4257a678fb6f4da",
		preview: null
	},
	{
		name: "7008.RW2",
		path: "7008/nice/Panasonic%20-%20DMC-LX7%20-%201%3A1.RW2",
		length: 3249664,
		sha256: "d142a23aca836053ed53e9ce3cb3ed2d434541d734d71a94a6eefadcd08bd31b",
		preview: { width: 1920, height: 1920 }
	}
]

/// `writeEmbeddedPreview` into an OPFS file — the shape a web caller uses, so the preview
/// never sits in wasm memory and the result is a File it can `createObjectURL`. Returns the
/// verdict and the file as written; the caller removes the entry.
async function writePreviewToOpfs(
	file: AnyFile,
	name: string,
	client: Client | UnauthClient = state
): Promise<{ result: EmbeddedPreviewResult; written: globalThis.File }> {
	const root = await navigator.storage.getDirectory()
	const handle = await root.getFileHandle(name, { create: true })
	const writer = await handle.createWritable()
	const result = await client.writeEmbeddedPreview({ file, writer })
	return { result, written: await handle.getFile() }
}

async function sha256Hex(bytes: Uint8Array<ArrayBuffer>): Promise<string> {
	const digest = await crypto.subtle.digest("SHA-256", bytes)
	return Array.from(new Uint8Array(digest), b => b.toString(16).padStart(2, "0")).join("")
}

test("raw thumbnails come from the embedded preview", { timeout: cap(600_000) }, async () => {
	const results = await Promise.all(
		RAW_FIXTURES.map(async fixture => {
			const res = await fetch(`raw-fixtures/${fixture.name}/${fixture.path}`)

			if (!res.ok) {
				console.warn(`SKIP ${fixture.name}: ${res.status} from the fixture cache`)

				return null
			}

			const bytes = new Uint8Array(await res.arrayBuffer())
			expect(bytes.length, `${fixture.name} length`).toBe(fixture.length)
			expect(await sha256Hex(bytes), `${fixture.name} does not match its pin`).toBe(fixture.sha256)

			const file = await state.uploadFile(bytes, { parent: testDir, name: fixture.name })
			// Decided from the name, whatever mime the upload stored.
			expect(file.canMakeThumbnail, `${fixture.name} canMakeThumbnail`).toBe(true)

			const thumb = expectThumbnail(await state.makeThumbnailInMemory({ file, maxHeight: 256, maxWidth: 256 }))
			expect(thumb.fromEmbeddedPreview, `${fixture.name} was not served from its embedded preview`).toBe(true)
			expect(thumb.width).toBeLessThanOrEqual(256)
			expect(thumb.height).toBeLessThanOrEqual(256)

			const bitmap = await createImageBitmap(new Blob([thumb.webpData], { type: "image/webp" }))
			expect(bitmap.width).toBe(thumb.width)
			expect(bitmap.height).toBe(thumb.height)
			bitmap.close()

			// The preview: the same embedded JPEG, handed out as stored rather than
			// shrunk to a thumbnail. Written into OPFS, read back as a File, and
			// decoded by the browser — which is the whole point of not transcoding.
			const opfsName = `${fixture.name}.preview.jpg`
			const { result, written } = await writePreviewToOpfs(file, opfsName)
			try {
				if (fixture.preview === null) {
					expect(result.type, `${fixture.name} should have no preview`).toBe("noPreview")
					expect(written.size).toBe(0)
				} else {
					expect(result.type, `${fixture.name} preview verdict`).toBe("preview")
					if (result.type !== "preview") {
						throw new Error("unreachable")
					}
					expect({ width: result.width, height: result.height }).toEqual(fixture.preview)
					expect(written.size).toBe(Number(result.bytes))
					// `imageOrientation: "none"`: the stored frame, so a spliced EXIF rotation
					// cannot swap the axes under the comparison.
					const preview = await createImageBitmap(written, { imageOrientation: "none" })
					expect(preview.width).toBe(result.width)
					expect(preview.height).toBe(result.height)
					preview.close()
				}
			} finally {
				await (await navigator.storage.getDirectory()).removeEntry(opfsName)
			}

			return `${fixture.name} ${thumb.width}x${thumb.height} preview=${fixture.preview ? `${fixture.preview.width}x${fixture.preview.height}` : "none"}`
		})
	)
	const produced = results.filter((r): r is string => r !== null)

	if (produced.length === 0) {
		console.warn("raw thumbnails: not one fixture was available; nothing was checked")
	}

	console.log("raw thumbnails:", produced.join(", "))
})

/// Removes an OPFS entry whose writable the SDK still holds, once the SDK's abort of that
/// stream has released the lock — within a bound, so a stream never aborted is a failure.
async function removeOnceReleased(root: FileSystemDirectoryHandle, name: string): Promise<void> {
	for (let attempt = 0; ; attempt++) {
		try {
			await root.removeEntry(name)

			return
		} catch (e) {
			if (attempt >= 100) {
				throw e
			}

			await new Promise(resolve => setTimeout(resolve, 50))
		}
	}
}

/// On wasm every decode and locate runs on ONE worker thread that blocks on chunk replies
/// from the caller's driver task. Aborting a preview mid-fetch drops that driver; the worker
/// must see its reply channel close and give the job up, not park on an answer that is
/// never coming with every later thumbnail queued behind it for a 60 s stall deadline. The
/// clock on the follow-up thumbnail is the assertion: a parked worker costs a minute.
test("an aborted preview does not park the decode worker", async () => {
	const fixture = RAW_FIXTURES.find(f => f.name === "7008.RW2")!
	const res = await fetch(`raw-fixtures/${fixture.name}/${fixture.path}`)

	if (!res.ok) {
		console.warn(`SKIP ${fixture.name}: ${res.status} from the fixture cache`)

		return
	}

	const bytes = new Uint8Array(await res.arrayBuffer())
	expect(await sha256Hex(bytes)).toBe(fixture.sha256)
	const file = await state.uploadFile(bytes, { parent: testDir, name: `abort-${fixture.name}` })

	const root = await navigator.storage.getDirectory()
	const opfsName = `abort-${fixture.name}.preview.jpg`
	const handle = await root.getFileHandle(opfsName, { create: true })
	const abortController = new AbortController()
	const aborted = state.writeEmbeddedPreview({
		file,
		writer: await handle.createWritable(),
		managedFuture: { abortSignal: abortController.signal }
	})
	// Straight away: the locate's first chunk fetch is where the driver is waiting, and where
	// a dropped driver used to leave the worker.
	abortController.abort()
	await expect(aborted).rejects.toThrow()
	// The SDK aborts the stream from its side once the producer is gone, which is what
	// releases the file's lock; that lands a tick after the rejection, and until it does
	// removeEntry answers NoModificationAllowedError. Waiting for it is also the check that
	// the abort happens at all.
	await removeOnceReleased(root, opfsName)

	const started = Date.now()
	expectThumbnail(await state.makeThumbnailInMemory({ file, maxHeight: 128, maxWidth: 128 }))
	// Not cap()-scaled: it only means something while it stays under the worker's 60 s stall deadline.
	expect(Date.now() - started, "a thumbnail after an aborted preview waited on a dead worker").toBeLessThan(30_000)
})

test("thumbnail verdicts are distinguishable", async () => {
	// The point of the verdicts: three different "no picture" answers that a UI
	// treats differently. Previously all three arrived as `undefined`, and a
	// transport failure arrived that way too.
	const notAnImage = new TextEncoder().encode("this is not an image at all, not even close")
	const bogus = await state.uploadFile(notAnImage, { parent: testDir, name: "bogus.png" })
	const bogusVerdict = await state.makeThumbnailInMemory({ file: bogus, maxHeight: 64, maxWidth: 64 })
	expect(bogusVerdict.type).toBe("unsupported")

	// Real PNG magic and a real IHDR, truncated mid-image: the bytes ARE a
	// format we decode, they are just broken — a different answer.
	const png = await generateImage(64, 64, "image/png")
	const truncated = png.slice(0, Math.floor(png.length / 3))
	const broken = await state.uploadFile(truncated, { parent: testDir, name: "broken.png" })
	const brokenVerdict = await state.makeThumbnailInMemory({ file: broken, maxHeight: 64, maxWidth: 64 })
	expect(brokenVerdict.type).toBe("corrupt")
})

/// Every JS file shape reads the same way, so every one of them thumbnails and previews: the
/// drive `File`, the stable-id-less `File` a shared-in folder's listing hands out, the
/// `SharedFile` of a file shared with you directly, and the `LinkedFile` behind a file link —
/// that last one through the `UnauthClient`, which is all a link viewer has. The Rust side
/// used to accept the drive shape only, and `canMakeThumbnail` existed on it only.
test("thumbnails and previews for shared and linked files", { timeout: cap(600_000) }, async () => {
	// The native suites reset contacts and shares on BOTH accounts under this lock (test-utils'
	// set_up_contact_no_add), and the nightly runs them alongside this suite — main account
	// first, then the share account, the order they take it in.
	using _contactLock = await state.acquireLock({ resource: "test:contact" })
	using _shareContactLock = await shareClient.acquireLock({ resource: "test:contact" })
	const dir = await state.createDir(testDir, "any-file-thumbs")
	const parrot = await state.uploadFile(await (await fetch("imgs/parrot.jpg")).bytes(), { parent: dir, name: "parrot.jpg" })
	const size = { maxHeight: 64, maxWidth: 64 }

	// A file link, read by an unauthenticated client ...
	const link = await state.publicLinkFile(parrot)
	const linked = await unauthClient.getLinkedFile(link.linkUuid, getFileMeta(parrot.meta)!.key, null)
	expect(linked.canMakeThumbnail).toBe(true)
	const linkedThumb = expectThumbnail(await unauthClient.makeThumbnailInMemory({ file: linked, ...size }))
	expect(linkedThumb.width).toBeLessThanOrEqual(64)
	expect(linkedThumb.height).toBeLessThanOrEqual(64)
	// ... and by a logged-in one, which is what a web user opening someone's link holds.
	expectThumbnail(await state.makeThumbnailInMemory({ file: linked, ...size }))

	// Shared with a contact: the file directly (a `SharedFile` at their shared-in root) and
	// its folder (a stable-id-less `File` in that folder's listing).
	const contact = await ensureShareContact()
	try {
		await state.shareFile(parrot, contact)
		await state.shareDir(dir, contact, () => {})
		const inShared = await shareClient.listInShared()
		const sharedFile = inShared.files.find(f => f.uuid === parrot.uuid)
		expect(sharedFile).toBeDefined()
		expect(sharedFile!.canMakeThumbnail).toBe(true)
		expectThumbnail(await shareClient.makeThumbnailInMemory({ file: sharedFile!, ...size }))

		const sharedDir = inShared.dirs.find(d => d.inner.uuid === dir.uuid)
		expect(sharedDir).toBeDefined()
		const listed = (await shareClient.listSharedDir(sharedDir!, sharedDir!.sharingRole)).files.find(f => f.uuid === parrot.uuid)
		expect(listed).toBeDefined()
		expect(listed!.stableUUID).toBeUndefined()
		expect(listed!.canMakeThumbnail).toBe(true)
		expectThumbnail(await shareClient.makeThumbnailInMemory({ file: listed!, ...size }))

		// The embedded preview through the same shapes, on a RAW fixture: linked (unauth) and
		// shared. Skipped, like the RAW test, when the volunteer-run library is unreachable.
		const fixture = RAW_FIXTURES.find(f => f.name === "7008.RW2")!
		const res = await fetch(`raw-fixtures/${fixture.name}/${fixture.path}`)
		if (!res.ok) {
			console.warn(`SKIP preview half: ${res.status} from the fixture cache`)
			return
		}
		const bytes = new Uint8Array(await res.arrayBuffer())
		expect(await sha256Hex(bytes)).toBe(fixture.sha256)
		const raw = await state.uploadFile(bytes, { parent: dir, name: `any-${fixture.name}` })
		const rawLink = await state.publicLinkFile(raw)
		const rawLinked = await unauthClient.getLinkedFile(rawLink.linkUuid, getFileMeta(raw.meta)!.key, null)
		await state.shareFile(raw, contact)
		const rawShared = (await shareClient.listInShared()).files.find(f => f.uuid === raw.uuid)
		expect(rawShared).toBeDefined()
		const root = await navigator.storage.getDirectory()
		const previews: [string, Client | UnauthClient, AnyFile][] = [
			["linked", unauthClient, rawLinked],
			["shared", shareClient, rawShared!]
		]
		for (const [label, client, file] of previews) {
			const opfsName = `any-${label}-${fixture.name}.preview.jpg`
			const { result, written } = await writePreviewToOpfs(file, opfsName, client)
			try {
				expect(result.type, `${label} preview verdict`).toBe("preview")
				if (result.type !== "preview") {
					throw new Error("unreachable")
				}
				expect({ width: result.width, height: result.height }).toEqual(fixture.preview)
				expect(written.size).toBe(Number(result.bytes))
			} finally {
				await root.removeEntry(opfsName)
			}
		}
	} finally {
		// Best effort: a throw here would replace the assertion that actually failed.
		await state.deleteContact(contact.uuid).catch(e => console.warn("deleteContact cleanup failed", e))
	}
})

test("a queued thumbnail is not expired by another caller's decode", async () => {
	// On wasm every decode runs on ONE worker thread that takes jobs strictly in turn, and each
	// caller arms its stall deadline the moment it QUEUES — deferring it to when its own job
	// starts would put a caller behind a trapped worker with no deadline at all, the exact hang
	// the deadline exists to break. So a caller routinely sits armed for far longer than its own
	// decode takes, and the only thing stopping it from declaring a perfectly healthy worker dead
	// is the process-global activity stamp the worker bumps for whoever it is currently serving.
	//
	// Reaching that state means keeping the queue busy past DECODE_STALL_TIMEOUT (60 s). Two
	// config knobs do it with no production change: `thumbnailDecodeConcurrency` admits every
	// caller at once so they all ARM (at the default of 2 the rest would be parked on the decode
	// permit, which is before the deadline exists and proves nothing), and `rateLimitPerSec: 1`
	// paces the single chunk request each decode makes, so the queue drains at ~1 job/s however
	// fast the machine is. The bandwidth knobs cannot do this — those tower layers are cfg'd out
	// on wasm.
	const QUEUED = 75
	const slow = UnauthClient.from_config({ rateLimitPerSec: 1, thumbnailDecodeConcurrency: QUEUED }).fromStringified(
		await state.toStringified()
	)

	// Under a chunk, so one rate-limited request per decode and the queue costs ~QUEUED seconds
	// and nothing else. Uploaded through `state`, which is not rate limited.
	const bytes = await generateImage(64, 64, "image/png")
	const file = await state.uploadFile(bytes, { parent: testDir, name: "queued-decode.png" })
	expect(file.canMakeThumbnail).toBe(true)

	const started = Date.now()
	const pending = Array.from({ length: QUEUED }, () => slow.makeThumbnailInMemory({ file, maxHeight: 64, maxWidth: 64 }))
	const results = await Promise.all(pending)
	const elapsed = Date.now() - started

	// Load-bearing precondition, asserted first: the last caller really did sit armed through at
	// least one 60 s expiry. If the queue ever drains faster than that this test has stopped
	// testing anything, and it must say so rather than pass.
	expect(elapsed).toBeGreaterThan(65000)
	// And every one of them still got a picture. A deadline that measured the EXPIRING caller's
	// own progress instead of the worker's would hand the late ones "thumbnail decode worker
	// died" here — and retire a live worker mid-decode on the way out.
	for (const result of results) {
		expectThumbnail(result)
	}
})

test("exif upload applies DateTimeOriginal", async () => {
	// Each fixture in test-assets/imgs has EXIF DateTimeOriginal=2020:06:15 10:30:00
	// injected via exiftool. nom-exif (the WASM-compatible fork) parses jpg, tiff,
	// heif and avif via the bytes-tee path inside `upload_file`; png and webp
	// carry the same EXIF on disk but nom-exif does not (yet) parse them.
	const EXIF_TIME = BigInt(Date.UTC(2020, 5, 15, 10, 30, 0))
	const formats = ["jpg", "tiff", "heif", "avif"]

	const results = await Promise.all(
		formats.map(async ext => {
			const res = await fetch(`imgs/parrot.${ext}`)
			const bytes = await res.bytes()
			const file = await state.uploadFile(bytes, {
				parent: testDir,
				name: `exif-parrot.${ext}`
			})
			const meta = getFileMeta(file.meta)
			return { ext, created: meta?.created, name: meta?.name }
		})
	)

	for (const r of results) {
		expect(r.name, `${r.ext}: file name preserved`).toBe(`exif-parrot.${r.ext}`)
		expect(r.created, `${r.ext}: created should equal EXIF DateTimeOriginal`).toStrictEqual(EXIF_TIME)
	}
})

test("exif upload respects noExif and noExifOverride flags", async () => {
	const EXIF_TIME = BigInt(Date.UTC(2020, 5, 15, 10, 30, 0))
	const USER_TIME = BigInt(Date.UTC(2015, 0, 1, 0, 0, 0))
	const before = BigInt(Date.now())
	const parrotImage = await fetch("imgs/parrot.jpg")
	const bytes = await parrotImage.bytes()

	// noExif: parser never runs. With no user-supplied created, the SDK falls
	// back to "now", so created should be >= the timestamp we captured before
	// the upload and not equal to the embedded EXIF time.
	const skipped = await state.uploadFile(bytes, {
		parent: testDir,
		name: "exif-noexif-parrot.jpg",
		noExif: true
	})
	const skippedMeta = getFileMeta(skipped.meta)
	expect(skippedMeta?.created, "noExif: created must not be EXIF time").not.toStrictEqual(EXIF_TIME)
	expect(skippedMeta?.created!, "noExif: created should be ~now").toBeGreaterThanOrEqual(before)

	// noExifOverride: parser still runs, but the user-supplied `created` wins
	// over the EXIF DateTimeOriginal.
	const preserved = await state.uploadFile(bytes, {
		parent: testDir,
		name: "exif-nooverride-parrot.jpg",
		created: USER_TIME,
		modified: USER_TIME,
		noExifOverride: true
	})
	const preservedMeta = getFileMeta(preserved.meta)
	expect(preservedMeta?.created, "noExifOverride: user-set created must win").toStrictEqual(USER_TIME)

	// Sanity: same fixture without flags still gets EXIF time applied. This
	// guards against the upload-pipeline path silently changing under us.
	const overridden = await state.uploadFile(bytes, {
		parent: testDir,
		name: "exif-default-parrot.jpg",
		created: USER_TIME,
		modified: USER_TIME
	})
	const overriddenMeta = getFileMeta(overridden.meta)
	expect(overriddenMeta?.created, "default: EXIF overrides user-set created").toStrictEqual(EXIF_TIME)
})

test("meta updates", async () => {
	const file = await state.uploadFile(new TextEncoder().encode("meta file content"), {
		parent: testDir,
		name: "meta-file.txt"
	})
	const meta = getFileMeta(file.meta)
	expect(meta?.name).toBe("meta-file.txt")
	expect(meta?.created).toBeDefined()
	expect(meta?.modified).toBeDefined()

	let updatedFile = await state.updateFileMetadata(file, {
		created: null
	})
	const updatedMeta = getFileMeta(updatedFile.meta)
	expect(updatedMeta?.created).toBeUndefined()

	updatedFile = await state.updateFileMetadata(file, {
		name: "meta-file-renamed.txt"
	})
	const renamedMeta = getFileMeta(updatedFile.meta)
	expect(renamedMeta?.name).toBe("meta-file-renamed.txt")

	const dir = await state.createDir(testDir, "meta-dir")
	const dirMeta = getDirMeta(dir.meta)
	expect(dirMeta?.name).toBe("meta-dir")
	expect(dirMeta?.created).toBeDefined()
	let updatedDir = await state.updateDirMetadata(dir, {
		created: null
	})
	const updatedDirMeta = getDirMeta(updatedDir.meta)
	expect(updatedDirMeta?.created).toBeUndefined()

	updatedDir = await state.updateDirMetadata(dir, {
		name: "meta-dir-renamed"
	})
	const renamedDirMeta = getDirMeta(updatedDir.meta)
	expect(renamedDirMeta?.name).toBe("meta-dir-renamed")

	// invalid names must reject with a normal error instead of aborting at the
	// FFI boundary
	let fileNameError: unknown
	try {
		await state.updateFileMetadata(updatedFile, {
			name: "bad/name.txt"
		})
	} catch (e) {
		fileNameError = e
	}
	expect(fileNameError).toBeInstanceOf(FilenSdkError)
	expect((fileNameError as FilenSdkError).kind).toBe("InvalidName")

	let dirNameError: unknown
	try {
		await state.updateDirMetadata(updatedDir, {
			name: "bad/dir"
		})
	} catch (e) {
		dirNameError = e
	}
	expect(dirNameError).toBeInstanceOf(FilenSdkError)
	expect((dirNameError as FilenSdkError).kind).toBe("InvalidName")

	const favFileResult = await state.setFavorite(updatedFile, true)
	if (favFileResult.type !== "file") {
		throw new Error("Expected setFavorite to return a File")
	}
	updatedFile = favFileResult
	const favDirResult = await state.setFavorite(updatedDir, true)
	if (favDirResult.type !== "dir") {
		throw new Error("Expected setFavorite to return a Dir")
	}
	updatedDir = favDirResult
	expect(updatedFile.favorited).toBe(true)
	expect(updatedDir.favorited).toBe(true)
})

test("color", async () => {
	let dir = await state.createDir(testDir, "color-dir")
	expect(dir.color).toBe("default")

	dir = await state.setDirColor(dir, "blue")
	expect(dir.color).toBe("blue")
	expect(dir).toEqual(await state.getDir(dir.uuid))

	dir = await state.setDirColor(dir, "green")
	expect(dir.color).toBe("green")
	expect(dir).toEqual(await state.getDir(dir.uuid))

	dir = await state.setDirColor(dir, "purple")
	expect(dir.color).toBe("purple")
	expect(dir).toEqual(await state.getDir(dir.uuid))

	dir = await state.setDirColor(dir, "red")
	expect(dir.color).toBe("red")
	expect(dir).toEqual(await state.getDir(dir.uuid))

	dir = await state.setDirColor(dir, "gray")
	expect(dir.color).toBe("gray")
	expect(dir).toEqual(await state.getDir(dir.uuid))

	dir = await state.setDirColor(dir, "#123456")
	expect(dir.color).toBe("#123456")
	expect(dir).toEqual(await state.getDir(dir.uuid))
})

test("notes", async () => {
	let note = await state.createNote()
	expect(note).toBeDefined()
	expect(note.uuid).toBeDefined()
	const fetchedNote = await state.getNote(note.uuid)
	expect(fetchedNote).toEqual(note)

	note = await state.setNoteContent(note, "This is the note content", "This is the preview")
	expect(note.preview).toBe("This is the preview")
	const content = await state.getNoteContent(note)
	expect(content).toBe("This is the note content")

	let tag = await state.createNoteTag("Test Tag")
	const resp = await state.addTagToNote(note, tag)
	note = resp.note
	tag = resp.tag
	expect(note.tags).toBeDefined()
	expect(note.tags!.length).toBe(1)
	expect(note.tags![0].uuid).toBe(tag.uuid)
	const tags = await state.listNoteTags()
	expect(tags.find(t => t.uuid === tag.uuid)).toBeDefined()

	const history = await state.getNoteHistory(note)
	expect(history.length).toBe(2)
	expect(history[0].preview).toBe("")
	expect(history[0].content).toBe("")
	expect(history[1].preview).toBe("This is the preview")
	expect(history[1].content).toBe("This is the note content")
})

test("chats", async () => {
	// Hold the same account-wide `test:chats` server lock the native chat tests take
	// (chat_tests.rs `lock_chat`): this suite shares the V2 account with the native matrix
	// legs, and unserialized conversation churn is what exhausts the server's
	// time-windowed create budget (`rate_limited` on v3/chat/conversations/create).
	// Released via `Symbol.dispose` at scope exit — after the deleteChat cleanup below.
	using _lock = await state.acquireLock({ resource: "test:chats" })
	let chat = await state.createChat([])
	expect(chat).toBeDefined()
	try {
		chat = await state.renameChat(chat, "Test Chat")
		expect(chat.name).toBe("Test Chat")

		chat = await state.sendChatMessage(chat, "This is a test message")
		expect(chat.lastMessage?.message).toEqual("This is a test message")
		const fetchedChat = await state.getChat(chat.uuid)
		expect(fetchedChat).toEqual(chat)

		// sleep for 5s
		await new Promise(resolve => setTimeout(resolve, 5000))

		const chatEvent = allEvents.find(e => e.type === "chat" && e.inner.type === "messageNew" && e.inner.msg.chat === chat.uuid)

		expect(chatEvent).toBeDefined()

		if (chatEvent?.type !== "chat" || chatEvent.inner.type !== "messageNew") {
			throw new Error("Expected chatMessageNew event")
		}

		expect(chatEvent.inner.msg).toEqual(fetchedChat?.lastMessage)
	} finally {
		// Delete the conversation even on failure — leaked chats are what exhaust the
		// create budget for later runs.
		await state.deleteChat(chat)
	}
})

test("authError", async () => {
	const badStringified = await state.toStringified()
	badStringified.apiKey = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
	const badState = unauthClient.fromStringified(badStringified)
	try {
		await badState.listDir(badState.root())
		expect.fail("Expected error to be thrown")
	} catch (e) {
		expect(e).toBeInstanceOf(FilenSdkError)
		expect((e as FilenSdkError).kind).toEqual("Unauthenticated")
		expect((e as FilenSdkError).toString()).toContain("v3/dir/content")
		// `message`/`name` are Error-shaped string PROPERTIES (wasm getters) so generic
		// renderers print "FilenSdkError: <message>" — a method here regresses uncaught
		// SDK errors back to "Unknown Error: Function<message>" in vitest.
		expect(typeof (e as FilenSdkError).message).toBe("string")
		expect((e as FilenSdkError).message).toContain("Unauthenticated")
		expect((e as FilenSdkError).name).toBe("FilenSdkError")
	}

	let gotAuthFailedEvent = false
	try {
		await badState.addEventListener(
			(event: SocketEvent) => {
				if (event.type === "authFailed") {
					gotAuthFailedEvent = true
				} else {
					throw new Error("Expected authFailed event")
				}
			},
			["authFailed"]
		)
		expect.fail("Expected error to be thrown")
	} catch (e) {
		expect(e).toBeInstanceOf(FilenSdkError)
		expect((e as FilenSdkError).kind).toEqual("Unauthenticated")
		expect((e as FilenSdkError).toString()).toContain("socket")
	}
	// The SDK guarantees the authFailed event is ENQUEUED before the registration
	// promise rejects — not that the JS listener has already run. The event is
	// drained by its own main-thread task, and nothing orders that task's poll
	// ahead of the rejection's, so asserting the flag synchronously in the catch
	// races the drain (and loses on slow firefox runners). Await it instead.
	await expect.poll(() => gotAuthFailedEvent, { timeout: 10_000 }).toBe(true)
})

test("sockets", async () => {
	expect(await state.isSocketConnected()).toBe(true)
	for (const handle of listenerHandles) {
		handle.free()
	}
	listenerHandles.length = 0
	expect(await state.isSocketConnected()).toBe(false)
	{
		/* eslint-disable @typescript-eslint/no-unused-vars */
		using _ = await state.addEventListener(() => {}, null)
		expect(await state.isSocketConnected()).toBe(true)
	}
	expect(await state.isSocketConnected()).toBe(false)
	// Re-arm the suite-wide capture freed above: without it the run is blind to
	// exactly the kind of external mutation of testDir this suite once suffered.
	listenerHandles.push(await state.addEventListener(suiteListener, null))
})

test("listLinkedItems", async () => {
	const dir = await state.createDir(testDir, "linked-items-dir")
	const file = await state.uploadFile(new TextEncoder().encode("linked file content"), {
		parent: dir,
		name: "linked-file.txt"
	})
	await state.publicLinkDir(dir, (downloaded, total) => {
		console.log("callback", downloaded, total)
	})
	let linkedItems = await state.listLinkedItems()
	const found = linkedItems.dirs.find(i => i.uuid === dir.uuid)
	expect(found).toBeDefined()
	expect(found).toEqual(dir)

	await state.publicLinkFile(file)
	linkedItems = await state.listLinkedItems()
	const foundFile = linkedItems.files.find(i => i.uuid === file.uuid)
	expect(foundFile).toBeDefined()
	expect(foundFile).toEqual(file)
})

test("Linked Dir Zip Download", async () => {
	const linkedZipDir = await state.createDir(testDir, "linked-zip-dir")
	const subDir = await state.createDir(linkedZipDir, "sub")

	const file1 = await state.uploadFile(new TextEncoder().encode("linked zip file 1"), {
		parent: linkedZipDir,
		name: "linked1.txt"
	})
	const file2 = await state.uploadFile(new TextEncoder().encode("linked zip file 2"), {
		parent: subDir,
		name: "linked2.txt"
	})

	// Create a public link for the directory
	const linkRW = await state.publicLinkDir(linkedZipDir, (downloaded, total) => {
		console.log("publicLinkDir progress", downloaded, total)
	})
	if (!linkRW.linkKey || linkRW.linkKeyVersion === undefined) {
		throw new Error("Expected linkRW to have a decrypted linkKey")
	}

	// Fetch the public link info via the unauthenticated client — this gives us
	// a DirPublicLink (read-only, with decrypted key) and the LinkedRootDir
	const linkInfo = await unauthClient.getDirPublicLinkInfo(linkRW.linkUuid, linkRW.linkKey)

	// Build AnyLinkedDirWithContext: the root dir paired with its public link
	const linkedDirWithContext: AnyLinkedDirWithContext = {
		dir: linkInfo.root,
		link: linkInfo.link
	}

	const { readable, writable } = new TransformStream<Uint8Array>()

	let lastBytesWritten = 0n
	let lastTotalBytes = 0n
	let progressCallCount = 0

	// Do not await here: TransformStream has no internal buffer, awaiting before consuming
	// the readable side would deadlock (the writer blocks when the reader is not draining).
	let downloadError: unknown = undefined
	const downloadPromise = unauthClient
		.downloadLinkedDirToZip(
			linkedDirWithContext,
			writable,
			(bytesWritten: bigint, totalBytes: bigint, _itemsProcessed: bigint, _totalItems: bigint) => {
				lastBytesWritten = bytesWritten
				lastTotalBytes = totalBytes
				progressCallCount++
			},
			{}
		)
		.catch((e: unknown) => {
			downloadError = e
		})

	const zipReader = new ZipReader<ReadableStream<Uint8Array>>(readable)

	let zipError: unknown = undefined
	const entries = await zipReader.getEntries().catch((e: unknown) => {
		zipError = e
		return [] as Entry[]
	})

	await downloadPromise
	if (downloadError !== undefined) {
		throw new Error(`downloadLinkedDirToZip failed: ${downloadError}`)
	}
	if (zipError !== undefined) {
		throw new Error(`ZipReader.getEntries failed: ${zipError}`)
	}

	console.log(entries)

	const map = new Map<string, Entry>()
	for (const entry of entries) {
		map.set(entry.filename, entry)
	}

	// Verify file1 at root of the linked dir
	const entry1 = map.get("linked1.txt")
	expect(entry1).toBeDefined()
	if (!entry1 || entry1.directory) throw new Error("entry1 not found or is a directory")
	const data1 = await entry1.getData(new Uint8ArrayWriter())
	expect(data1).toEqual(new TextEncoder().encode("linked zip file 1"))
	expect(BigInt(entry1.uncompressedSize)).toEqual(file1.size)

	// Verify file2 inside the sub-directory
	const entry2 = map.get("sub/linked2.txt")
	expect(entry2).toBeDefined()
	if (!entry2 || entry2.directory) throw new Error("entry2 not found or is a directory")
	const data2 = await entry2.getData(new Uint8ArrayWriter())
	expect(data2).toEqual(new TextEncoder().encode("linked zip file 2"))
	expect(BigInt(entry2.uncompressedSize)).toEqual(file2.size)

	// Verify progress callbacks fired
	expect(progressCallCount).toBeGreaterThan(0)
	expect(lastBytesWritten).toBeGreaterThan(0n)
	expect(lastBytesWritten).toBeLessThanOrEqual(lastTotalBytes)
})

test("favorites", async () => {
	let dir = await state.createDir(testDir, "favorites-dir")
	let file = await state.uploadFile(new TextEncoder().encode("favorites file content"), {
		parent: testDir,
		name: "favorites-file.txt"
	})

	let favorites = await state.listFavorites()

	expect(favorites.dirs.find(i => i.uuid === dir.uuid)).toBeUndefined()
	expect(favorites.files.find(i => i.uuid === file.uuid)).toBeUndefined()

	const setDir = await state.setFavorite(dir, true)
	if (setDir.type !== "dir") {
		throw new Error("Expected setFavorite to return a Dir")
	}
	dir = setDir
	const setFile = await state.setFavorite(file, true)
	if (setFile.type !== "file") {
		throw new Error("Expected setFavorite to return a File")
	}
	file = setFile

	favorites = await state.listFavorites()
	const foundDir = favorites.dirs.find(i => i.uuid === dir.uuid)
	expect(foundDir).toBeDefined()
	expect(dir).toMatchObject(foundDir as Dir)

	const foundFile = favorites.files.find(i => i.uuid === file.uuid)
	expect(foundFile).toBeDefined()
	expect(file).toMatchObject(foundFile as File)
})

test("getFileByStableUuidOptional follows a lineage across edits", async () => {
	const name = "stable-lineage.txt"
	const first = await state.uploadFile(new TextEncoder().encode("v1"), { parent: testDir, name })
	// backend timestamps have a resolution of one second
	await new Promise(resolve => setTimeout(resolve, 2000))
	const edited = await state.uploadFile(new TextEncoder().encode("edited"), { parent: testDir, name })
	expect(edited.uuid).not.toBe(first.uuid)
	expect(edited.stableUUID).toBe(first.stableUUID)

	const head = await state.getFileByStableUuidOptional(first.stableUUID!)
	expect(head?.uuid).toBe(edited.uuid)
	expect(head?.stableUUID).toBe(first.stableUUID)
	expect(new TextDecoder().decode(await state.downloadFile(head!))).toBe("edited")

	await state.deleteFilePermanently(edited)
	expect(await state.getFileByStableUuidOptional(first.stableUUID!)).toBeUndefined()
})

test("service worker", async () => {
	if (!("serviceWorker" in navigator)) {
		throw new Error("Service workers are not supported in this environment")
	}

	const serviceWorker = await window.navigator.serviceWorker.register("/sw.js", {
		scope: "/",
		type: "classic"
	})

	await serviceWorker.update()

	const intervalId = setInterval(() => {
		console.log(Date.now(), "Service worker state:", serviceWorker.active?.state)
	}, 1000)

	try {
		if (!serviceWorker || !serviceWorker.active) {
			throw new Error("Service worker is not active")
		}

		await new Promise<void>(resolve => {
			;(async () => {
				while (!serviceWorker.active?.state || serviceWorker.active.state !== "activated") {
					await new Promise<void>(resolve => setTimeout(resolve, 100))
				}

				resolve()
			})()
		})

		// wait a bit to ensure service worker is ready and wasm is loaded
		await new Promise<void>(resolve => setTimeout(resolve, 5000))

		const jsonClient = JSON.stringify(await state.toStringified(), jsonBigIntReplacer)
		const clientParam = `stringifiedClient=${encodeURIComponent(jsonClient)}`

		// The worker answers its own failures with a 500 whose body names them; surface that body,
		// a bare `res.ok` assertion never does.
		const bodyOrThrow = async (res: Response, what: string) => {
			const text = await res.text()

			if (!res.ok) {
				throw new Error(`${what} answered ${res.status}: ${text}`)
			}

			return text
		}

		await bodyOrThrow(await fetch(`/serviceWorker/init?${clientParam}`), "service worker init")

		// wait a bit to ensure service worker is ready and client is loaded
		await new Promise<void>(resolve => setTimeout(resolve, 5000))

		const file = await state.uploadFile(new TextEncoder().encode("service worker file content"), {
			parent: testDir,
			name: "sw-file.txt"
		})

		const stringifiedFile = JSON.stringify(file, jsonBigIntReplacer)

		// Firefox stops a service worker that has been idle for 30 s and starts a fresh one, with
		// empty module state, on the next fetch. In the nightly the upload above waits on the
		// drive-write lock for longer than that, so idle past the timeout on purpose: every run then
		// downloads through a restarted worker, which re-initialises from the client on the request.
		await new Promise<void>(resolve => setTimeout(resolve, 35_000))

		const text = await bodyOrThrow(
			await fetch(`/serviceWorker/download?file=${encodeURIComponent(stringifiedFile)}&${clientParam}`),
			"service worker download"
		)
		expect(text).toBe("service worker file content")
	} finally {
		clearInterval(intervalId)
	}
})

test("name validation", () => {
	// Helper: call parseName and return the error kind, or fail if it didn't throw
	function expectErrorKind(name: string, expectedKind: string) {
		try {
			parseName(name)
			expect.fail(`Expected parseName(${JSON.stringify(name)}) to throw, but it returned successfully`)
		} catch (e: unknown) {
			const err = e as EntryNameErrorJS
			expect(err.kind()).toBe(expectedKind)
			expect(err.name()).toBe(name)
			expect(err.message()).toBeTruthy()
		}
	}

	// Helper: generate all 2^n case combinations for an ASCII string
	function allCaseCombinations(s: string): string[] {
		const chars = s.split("")
		const n = chars.length
		const results: string[] = []
		for (let mask = 0; mask < 1 << n; mask++) {
			results.push(chars.map((ch, i) => (mask & (1 << i) ? ch.toUpperCase() : ch.toLowerCase())).join(""))
		}
		return results
	}

	// ── Valid simple names ──
	for (const name of ["hello", "file.txt", "my-document.pdf", "image_001.png", "a", "ab"]) {
		expect(parseName(name)).toBe(name)
	}

	// ── Valid unicode names ──
	for (const name of ["日本語.txt", "über.doc", "café", "файл.txt", "🎉"]) {
		const result = parseName(name)
		expect(result).toBeDefined()
	}

	// ── Valid names with dots ──
	for (const name of ["file.tar.gz", ".hidden", ".gitignore", "a.b.c.d"]) {
		expect(parseName(name)).toBe(name)
	}

	// ── Valid at max length (255 bytes) ──
	expect(parseName("a".repeat(255))).toBe("a".repeat(255))

	// ── Empty ──
	expectErrorKind("", "Empty")

	// ── Dot entries ──
	expectErrorKind(".", "DotEntry")
	expectErrorKind("..", "DotEntry")

	// ── Too long ──
	expectErrorKind("a".repeat(256), "TooLong")
	// Multibyte: 🎉 is 4 UTF-8 bytes, 64 × 4 = 256 > 255
	expectErrorKind("🎉".repeat(64), "TooLong")

	// ── Leading space ──
	expectErrorKind(" foo", "LeadingSpace")
	expectErrorKind("  bar", "LeadingSpace")
	expectErrorKind(" ", "LeadingSpace")

	// ── Trailing dot or space ──
	expectErrorKind("foo.", "TrailingDotOrSpace")
	expectErrorKind("foo..", "TrailingDotOrSpace")
	expectErrorKind("foo ", "TrailingDotOrSpace")
	expectErrorKind("foo  ", "TrailingDotOrSpace")

	// ── Forbidden special characters ──
	for (const ch of ["/", "\\", ":", "*", "?", '"', "<", ">", "|"]) {
		expectErrorKind(`file${ch}name`, "ForbiddenChar")
	}

	// ── Forbidden control characters (0x01–0x1F) ──
	for (let byte = 1; byte <= 0x1f; byte++) {
		expectErrorKind(`file${String.fromCharCode(byte)}name`, "ForbiddenChar")
	}

	// ── Forbidden DEL (0x7F) ──
	expectErrorKind("file\x7fname", "ForbiddenChar")

	// ── Reserved names — all case combinations ──
	for (const base of ["con", "prn", "aux", "nul"]) {
		for (const variant of allCaseCombinations(base)) {
			expectErrorKind(variant, "ReservedName")
		}
	}

	// ── COM0–COM9, all case combinations ──
	for (let digit = 1; digit <= 9; digit++) {
		for (const variant of allCaseCombinations(`com${digit}`)) {
			expectErrorKind(variant, "ReservedName")
		}
	}

	// ── LPT0–LPT9, all case combinations ──
	for (let digit = 1; digit <= 9; digit++) {
		for (const variant of allCaseCombinations(`lpt${digit}`)) {
			expectErrorKind(variant, "ReservedName")
		}
	}

	// ── Reserved names with extensions (should be accepted) ──
	for (const name of [
		"CON.txt",
		"con.txt",
		"Con.log",
		"PRN.txt",
		"prn.doc",
		"AUX.dat",
		"aux.bin",
		"NUL.txt",
		"nul.csv",
		"COM1.txt",
		"com1.log",
		"COM9.txt",
		"LPT1.txt",
		"lpt1.dat",
		"LPT9.bin"
	]) {
		expect(parseName(name)).toBe(name)
	}

	// ── Not-reserved lookalikes (should be accepted) ──
	for (const name of [
		"CONSOLE",
		"PRINT",
		"AUXILIARY",
		"NULL",
		"COMA",
		"LPTA",
		"COM",
		"LPT",
		"CO",
		"LP",
		"CONX",
		"PRNX",
		"AUXX",
		"NULX"
	]) {
		expect(parseName(name)).toBe(name)
	}

	// ── NFC normalization ──
	// é as e + combining acute (NFD) should normalize to single codepoint (NFC)
	const nfd = "e\u0301" // NFD: e + combining acute accent
	const nfc = "\u00E9" // NFC: é as a single codepoint
	expect(parseName(nfd)).toBe(nfc)

	// Already-NFC input stays unchanged
	expect(parseName("café")).toBe("café")
})

test("name encoding", () => {
	// Helper: encoding must produce the expected valid name and decode back
	function expectRoundTrip(name: string, expectedEncoded: string) {
		const encoded = encodeName(name)
		expect(encoded).toBe(expectedEncoded)
		expect(parseName(encoded)).toBe(encoded)
		expect(decodeName(encoded)).toBe(name)
	}

	// ── Forbidden characters become fullwidth variants ──
	expectRoundTrip("a/b", "a／b")
	expectRoundTrip("a\\b", "a＼b")
	expectRoundTrip("a:b", "a：b")
	expectRoundTrip("a*b", "a＊b")
	expectRoundTrip("a?b", "a？b")
	expectRoundTrip('a"b', "a＂b")
	expectRoundTrip("a<b", "a＜b")
	expectRoundTrip("a>b", "a＞b")
	expectRoundTrip("a|b", "a｜b")

	// ── Control characters become Control Pictures symbols ──
	expectRoundTrip("a\x00b", "a␀b")
	expectRoundTrip("a\x1fb", "a␟b")
	expectRoundTrip("a\x7fb", "a␡b")

	// ── Leading/trailing spaces and trailing dots ──
	expectRoundTrip(" a", "␠a")
	expectRoundTrip("a ", "a␠")
	expectRoundTrip("a.", "a．")
	expectRoundTrip(" ", "␠")
	expectRoundTrip(".", "．")
	expectRoundTrip("..", "．．")

	// ── Reserved Windows device names ──
	expectRoundTrip("CON", "ＣON")
	expectRoundTrip("com1", "ｃom1")

	// ── Literal replacement characters get quoted ──
	expectRoundTrip("＊", "‛＊")
	expectRoundTrip("‛", "‛‛")
	expectRoundTrip("ＣON", "‛ＣON")

	// ── Valid names pass through untouched ──
	for (const name of ["hello.txt", ".hidden", "日本語.txt", "café", "CON.txt", "a b"]) {
		expectRoundTrip(name, name)
	}

	// ── decodeName round-trips the NFC form of non-NFC input ──
	const nfdColon = "e\u0301:x" // NFD: e + combining acute accent
	expect(decodeName(encodeName(nfdColon))).toBe(nfdColon.normalize("NFC"))

	// ── Errors carry the kind and offending name ──
	for (const [name, kind] of [
		["", "Empty"],
		[":".repeat(86), "TooLong"]
	]) {
		try {
			encodeName(name)
			expect.fail(`Expected encodeName(${JSON.stringify(name)}) to throw`)
		} catch (e: unknown) {
			const err = e as EntryNameErrorJS
			expect(err.kind()).toBe(kind)
			expect(err.name()).toBe(name)
		}
	}
})

test("getItemPath", async () => {
	// Create a two-level hierarchy: testDir -> path-parent -> path-child (dir) and path-parent -> path-file.txt
	const parentDir = await state.createDir(testDir, "path-parent")
	const childDir = await state.createDir(parentDir, "path-child")
	const childFile = await state.uploadFile(new TextEncoder().encode("path test content"), {
		parent: parentDir,
		name: "path-file.txt"
	})

	// Test nested dir: path ends with "/" and includes ancestors + own name
	const dirResult = await state.getItemPath(childDir)
	expect(dirResult.path).toBe(`${testDirName}/path-parent/path-child/`)
	expect(dirResult.ancestors).toBeInstanceOf(Array)
	expect(dirResult.ancestors.length).toBe(2)
	expect(getDirMeta(dirResult.ancestors[1].meta)?.name).toBe("path-parent")

	// Test nested file: path does NOT end with "/" and includes ancestors + own name
	const fileResult = await state.getItemPath(childFile)
	expect(fileResult.path).toBe(`${testDirName}/path-parent/path-file.txt`)
	expect(fileResult.ancestors).toBeInstanceOf(Array)
	expect(fileResult.ancestors.length).toBe(2)
	expect(getDirMeta(fileResult.ancestors[1].meta)?.name).toBe("path-parent")

	// Test item directly under root:
	const topLevelDirResult = await state.getItemPath(testDir)
	expect(topLevelDirResult.path).toBe(`${testDirName}/`)
	expect(topLevelDirResult.ancestors.length).toBe(0)
})

test("cache search", async () => {
	const statusMessages: CacheStatusMessage[] = []
	await state.configureCache("wasm-test-cache.db", (messages: CacheStatusMessage[]) => {
		statusMessages.push(...messages)
	})

	const searchDir = await state.createDir(testDir, "cache-search-dir")
	await state.uploadFile(new TextEncoder().encode("a"), {
		parent: searchDir,
		name: "alpha.txt"
	})
	await state.uploadFile(new TextEncoder().encode("b"), {
		parent: searchDir,
		name: "Beta.txt"
	})

	// An uncovered root: the worker spawns, validates remotely, and runs a convergence resync
	// (whose progress lands on the status listener).
	const search = await state.createSearch(searchDir.uuid, {
		name: undefined,
		itemType: undefined,
		recursive: true,
		caseSensitive: false
	})

	const poll = async (predicate: () => boolean | Promise<boolean>, timeoutMs: number) => {
		const deadline = Date.now() + timeoutMs
		while (Date.now() < deadline) {
			if (await predicate()) return true
			await new Promise(resolve => setTimeout(resolve, 500))
		}
		return false
	}

	expect(await poll(async () => (await search.total()) === 2n, 90000)).toBe(true)

	const snapshots: CacheSearchSnapshot[] = []
	const window = await search.getRange(0n, 10n, (snapshot: CacheSearchSnapshot) => {
		snapshots.push(snapshot)
	})
	const initial = window.initialSnapshot()
	expect(initial).toBeDefined()
	expect(initial?.total).toBe(2n)
	expect(initial?.live).toBe(true)
	const names = initial?.results.map(hit => (hit.result.type === "file" ? getFileMeta(hit.result.file.meta)?.name : null))
	expect(names).toStrictEqual(["alpha.txt", "Beta.txt"])
	// Both files are direct children of the search root.
	expect(initial?.results.map(hit => hit.parentPath)).toStrictEqual(["", ""])
	// Consumed on first read.
	expect(window.initialSnapshot()).toBeUndefined()

	// A live upload pings the engine; the window listener delivers a fresh snapshot.
	await state.uploadFile(new TextEncoder().encode("c"), {
		parent: searchDir,
		name: "gamma.txt"
	})
	expect(await poll(() => snapshots.some(snapshot => snapshot.total === 3n && snapshot.live), 120000)).toBe(true)

	// Engine-local refilter.
	await search.setConfig({
		name: "beta",
		itemType: undefined,
		recursive: true,
		caseSensitive: false
	})
	expect(await search.total()).toBe(1n)

	// The uncovered add ran a resync; its progress arrived on the status listener.
	expect(await poll(() => statusMessages.some(message => message.type === "resyncProgress"), 60000)).toBe(true)

	await search.close()
	expect(await search.isLive()).toBe(false)
	window.free()
	search.free()
})

function nameOf(item: { meta: FileMeta } | { meta: DirMeta }): string | undefined {
	const meta = item.meta
	return meta.type === "decoded" ? meta.data.name : undefined
}

/// A source tree for the copy tests: `source/top.txt` and `source/sub/big.bin` (3 chunks + 7).
async function copySource(parent: Dir) {
	const source = await state.createDir(parent, "source")
	const sub = await state.createDir(source, "sub")
	const top = await state.uploadFile(new TextEncoder().encode("top"), { parent: source, name: "top.txt" })
	const big = await state.uploadFile(new Uint8Array(3 * 1024 * 1024 + 7).fill(7), { parent: sub, name: "big.bin" })
	return { source, sub, top, big }
}

test("copyItems copies a tree and delivers every callback in order before it resolves", async () => {
	const parent = await state.createDir(testDir, "copy-tree")
	const { source, big } = await copySource(parent)
	const destination = await state.createDir(parent, "destination")

	const copied = callbackLog()
	const { log } = copied
	const updates: CopyUpdate[] = []
	const report = await state.copyItems({
		items: [source],
		destination,
		onTopLevelPlanned: items => copied.note(`planned:${items.length}`),
		onTopLevelCreated: item => copied.note(`created:${item.request}`),
		onUpdate: update => {
			updates.push(update)
			copied.note(`update:${update.phase}`)
		}
	})

	expect(await copied.resolved()).toBe(0)
	expect(report.error).toBeUndefined()
	expect(report.failures).toHaveLength(0)
	expect(report.topLevel).toHaveLength(1)
	expect(report.counts.filesDone).toBe(2n)
	expect(report.counts.dirsCreated).toBe(2n)
	// scanning updates come first; the plan is announced before anything is created
	const planned = log.indexOf("planned:1")
	expect(planned).toBeGreaterThan(-1)
	expect(log.indexOf("created:0")).toBeGreaterThan(planned)
	expect(log.indexOf("update:creatingDirectories")).toBeGreaterThan(planned)
	expect(log[log.length - 1]).toBe("update:done")
	const last = updates[updates.length - 1]
	expect(last.counts).toStrictEqual(report.counts)
	expect(last.etaMs).toBe(0n)
	// every event arrives once, in a single ordered stream
	const created = updates.flatMap(u => u.events).filter(e => e.type === "dirCreated")
	expect(created.map(e => e.name)).toStrictEqual(["source", "sub"])

	const { dirs } = await state.listDir(destination)
	const copy = dirs.find(d => getDirMeta(d.meta)?.name === "source")
	expect(copy?.uuid).toBe(report.topLevel[0].item.uuid)
	const { dirs: copiedSubs } = await state.listDir(copy!)
	const { files: copiedBig } = await state.listDir(copiedSubs[0])
	expect(await state.downloadFile(copiedBig[0])).toStrictEqual(await state.downloadFile(big))
})

test("copyItemsTo copies into several destinations under their names", async () => {
	const parent = await state.createDir(testDir, "copy-to")
	const file = await state.uploadFile(new TextEncoder().encode("one file"), { parent, name: "x.txt" })
	const first = await state.createDir(parent, "first")
	const second = await state.createDir(parent, "second")
	await state.uploadFile(new TextEncoder().encode("taken"), { parent: second, name: "y.txt" })

	const report = await state.copyItemsTo({
		entries: [
			{ item: file, destination: first },
			{ item: file, destination: second, name: "y.txt" }
		]
	})
	expect(report.error).toBeUndefined()
	const names = report.topLevel.map(t => [t.request, nameOf(t.item)])
	expect(names).toContainEqual([0n, "x.txt"])
	expect(names).toContainEqual([1n, "y (1).txt"])
	const { files } = await state.listDir(second)
	expect(files.map(f => getFileMeta(f.meta)?.name).sort()).toStrictEqual(["y (1).txt", "y.txt"])
})

test("copyItems pauses, resumes and cancels through managedFuture", async () => {
	const parent = await state.createDir(testDir, "copy-controls")
	const { source } = await copySource(parent)
	for (let i = 0; i < 4; i++) {
		await state.uploadFile(new Uint8Array(2 * 1024 * 1024).fill(i), { parent: source, name: `more-${i}.bin` })
	}

	// paused as soon as the copy's directory exists, then resumed
	const pauseSignal = new PauseSignal()
	const updates: CopyUpdate[] = []
	const paused = state.copyItems({
		items: [source],
		destination: await state.createDir(parent, "paused"),
		onTopLevelCreated: () => pauseSignal.pause(),
		onUpdate: update => updates.push(update),
		managedFuture: { pauseSignal }
	})
	await vi.waitFor(
		() => {
			expect(updates.some(u => u.runState === "paused")).toBe(true)
		},
		{ timeout: cap(60_000), interval: 100 }
	)
	const doneWhilePaused = updates[updates.length - 1].counts.filesDone
	await new Promise(resolve => setTimeout(resolve, 2000))
	expect(updates[updates.length - 1].counts.filesDone).toBe(doneWhilePaused)
	pauseSignal.resume()
	const resumed = await paused
	expect(resumed.error).toBeUndefined()
	expect(resumed.counts.filesDone).toBe(6n)

	// aborted: the copy winds down and still reports what it created
	const controller = new AbortController()
	const cancelled = await state.copyItems({
		items: [source],
		destination: await state.createDir(parent, "cancelled"),
		onTopLevelPlanned: () => controller.abort(),
		managedFuture: { abortSignal: controller.signal }
	})
	expect(cancelled.error).toBeInstanceOf(FilenSdkError)
	expect(cancelled.error?.kind).toBe("Cancelled")
	expect(cancelled.counts.filesDone + cancelled.counts.filesFailed + cancelled.counts.filesNotAttempted).toBe(cancelled.totals.files)
})

test("a copy failure's item and parent can be passed back to copyItemsTo", async () => {
	const parent = await state.createDir(testDir, "copy-retry")
	const real = await state.uploadFile(new TextEncoder().encode("real"), { parent, name: "real.txt" })
	// the same file under a uuid the server holds no chunks for
	const missing = { ...real, uuid: crypto.randomUUID() }
	const destination = await state.createDir(parent, "destination")

	const updates: CopyUpdate[] = []
	const report = await state.copyItems({ items: [missing, real], destination, onUpdate: update => updates.push(update) })
	expect(report.error).toBeUndefined()
	expect(report.failures).toHaveLength(1)
	const [failure] = report.failures
	expect(failure.info.stage.type).toBe("download")
	expect(failure.info.error).toBeInstanceOf(FilenSdkError)
	expect(failure.info.error.kind).toBe("FileChunkNotFound")
	// the update's event carries the same error, read the same, though the report still held it then
	const failed = updates.flatMap(u => u.events).filter(e => e.type === "fileFailed")
	expect(failed).toHaveLength(1)
	expect(failed[0].error).toBeInstanceOf(FilenSdkError)
	expect(failed[0].error.kind).toBe("FileChunkNotFound")
	expect(failed[0].error.message).toBe(failure.info.error.message)
	expect(failed[0].error.inner_message()).toBe(failure.info.error.inner_message())
	expect(failure.info.destParent).toBe(destination.uuid)
	expect(failure.info.destParentDir.uuid).toBe(destination.uuid)
	expect((failure.item as { uuid: string }).uuid).toBe(missing.uuid)

	// the SDK reads back what it handed out: the retry fails the same way, at the same place
	const retry = await state.copyItemsTo({
		entries: [{ item: failure.item, destination: failure.info.destParentDir, name: "retried.txt" }]
	})
	expect(retry.error).toBeUndefined()
	expect(retry.failures).toHaveLength(1)
	expect(retry.failures[0].info.error.kind).toBe("FileChunkNotFound")
	expect(retry.failures[0].info.destParent).toBe(destination.uuid)

	// a created directory, as reported, is a copy source too
	const dir = await state.createDir(parent, "dir")
	const first = await state.copyItems({ items: [dir], destination })
	const copied = first.topLevel[0].item
	expect(copied.type).toBe("dir")
	const again = await state.copyItems({ items: [copied as AnyItemWithContext], destination })
	expect(again.error).toBeUndefined()
	expect(nameOf(again.topLevel[0].item)).toBe("dir (1)")
})

/// Bytes no codec shrinks to nothing, different for every `seed`, so an archive's entries can
/// only come back right if every byte made it through.
function fixtureBytes(length: number, seed: number): Uint8Array<ArrayBuffer> {
	const bytes = new Uint8Array(length)
	let x = (seed * 2654435761 + 1) >>> 0
	for (let i = 0; i < length; i++) {
		x ^= x << 13
		x ^= x >>> 17
		x ^= x << 5
		x >>>= 0
		bytes[i] = x & 0xff
	}
	return bytes
}

/// A tree for the archive tests: `archived/notes.txt`, `archived/sub/data.bin` (a chunk and a
/// bit) and the empty `archived/empty`.
async function archiveSource(parent: Dir, seed: number) {
	const root = await state.createDir(parent, "archived")
	const sub = await state.createDir(root, "sub")
	await state.createDir(root, "empty")
	const notes = new TextEncoder().encode(`notes for archive ${seed}`)
	const data = fixtureBytes(1024 * 1024 + 13, seed)
	await state.uploadFile(notes, { parent: root, name: "notes.txt" })
	await state.uploadFile(data, { parent: sub, name: "data.bin" })
	return { root, notes, data }
}

type ArchiveSource = Awaited<ReturnType<typeof archiveSource>>

async function childDir(parent: Dir, name: string): Promise<Dir> {
	const dir = (await state.listDir(parent)).dirs.find(d => nameOf(d) === name)
	if (!dir) {
		throw new Error(`no directory ${name} in ${nameOf(parent)}`)
	}
	return dir
}

async function childFile(parent: Dir, name: string): Promise<File> {
	const file = (await state.listDir(parent)).files.find(f => nameOf(f) === name)
	if (!file) {
		throw new Error(`no file ${name} in ${nameOf(parent)}`)
	}
	return file
}

/// `folder` holds `source` as extracted: its directories, and its files byte for byte.
async function expectExtracted(folder: Dir, source: ArchiveSource) {
	const root = await childDir(folder, "archived")
	expect(await state.downloadFile(await childFile(root, "notes.txt"))).toStrictEqual(source.notes)
	expect(await state.downloadFile(await childFile(await childDir(root, "sub"), "data.bin"))).toStrictEqual(source.data)
	expect(await state.listDir(await childDir(root, "empty"))).toMatchObject({ dirs: [], files: [] })
}

/// The callbacks of one job, in the order they came, and how many came after its call
/// resolved (there must be none: the SDK delivers everything a job reported first).
function callbackLog() {
	const log: string[] = []
	let resolved = false
	let late = 0
	return {
		log,
		note(entry: string) {
			late += resolved ? 1 : 0
			log.push(entry)
		},
		/// Marks the call resolved, then makes one more round trip through the SDK's worker: its
		/// answer reaches this thread after anything the job posted before it, so a callback
		/// still on its way has landed, and been counted, by the time this returns.
		async resolved() {
			resolved = true
			await state.isSocketConnected()
			return late
		}
	}
}

const tarGz: CompressFormat = { type: "tar", compression: { codec: "gzip", level: 6 } }

const ROUND_TRIPS: { label: string; name: string; format: CompressFormat; password?: string }[] = [
	{ label: "tar.gz", name: "photos.tar.gz", format: tarGz },
	{
		label: "AES-256 zip",
		name: "photos.zip",
		format: { type: "zip", method: { type: "deflate", level: 6 }, encryption: "aes256" },
		password: "zip pässword"
	},
	{
		label: "7z with encrypted headers",
		name: "photos.7z",
		format: { type: "sevenZ", method: { type: "lzma2", level: 5 }, solid: true, encryption: "entriesAndHeaders" },
		password: "7z password"
	},
	{ label: "tar.zst", name: "photos.tar.zst", format: { type: "tar", compression: { codec: "zstd" } } }
]

for (const [seed, trip] of ROUND_TRIPS.entries()) {
	test(`${trip.label} round-trips through compressItems and extractArchive, every callback before it resolves`, async () => {
		const parent = await state.createDir(testDir, `archive-${trip.label.replace(/\W+/g, "-")}`)
		const source = await archiveSource(parent, seed)
		expect(trip.name.endsWith(archiveExtension(trip.format))).toBe(true)

		const compressed = callbackLog()
		const compressUpdates: CompressUpdate[] = []
		const report = await state.compressItems(
			{
				items: [source.root],
				destination: parent,
				name: trip.name,
				format: trip.format,
				onUpdate: update => {
					compressed.note(`update:${update.phase}`)
					compressUpdates.push(update)
				},
				onArchiveCreated: archive => compressed.note(`created:${nameOf(archive)}`)
			},
			trip.password
		)
		expect(await compressed.resolved()).toBe(0)
		expect(report.error).toBeUndefined()
		expect(report.counts.filesDone).toBe(2n)
		const archive = report.archive!
		expect(nameOf(archive)).toBe(trip.name)
		// the archive is announced once it is registered, after the updates of writing it
		expect(compressed.log).toContain("update:compressing")
		const created = compressed.log.indexOf(`created:${trip.name}`)
		expect(created).toBeGreaterThan(compressed.log.indexOf("update:compressing"))
		expect(compressed.log.at(-1)).toBe("update:done")
		expect(compressUpdates.at(-1)!.counts).toStrictEqual(report.counts)

		const extracted = callbackLog()
		const topLevel: ExtractedTopLevelItem[] = []
		const extractUpdates: ExtractUpdate[] = []
		const extractReport = await state.extractArchive(
			{
				archive,
				destination: parent,
				root: { type: "newFolder" },
				onTopLevelBatch: items => {
					extracted.note("topLevel")
					topLevel.push(...items)
				},
				onUpdate: update => {
					extracted.note(`update:${update.counts.dirsCreated}`)
					extractUpdates.push(update)
				}
			},
			trip.password
		)
		expect(await extracted.resolved()).toBe(0)
		expect(extractReport.error).toBeUndefined()
		expect(extractReport.failures).toHaveLength(0)
		expect(extractReport.counts.filesDone).toBe(2n)
		// the new folder is named after the archive, and announced before an update counts it
		const [folder] = extractReport.topLevel
		expect(folder.key).toStrictEqual({ type: "root" })
		expect(nameOf(folder.item)).toBe(archiveDefaultName(trip.name))
		expect(topLevel.map(item => item.item.uuid)).toStrictEqual([folder.item.uuid])
		expect(extracted.log.indexOf("topLevel")).toBeLessThan(
			extracted.log.findIndex(entry => entry !== "update:0" && entry !== "topLevel")
		)
		expect(extractUpdates.at(-1)!.phase).toBe("done")
		expect(extractUpdates.at(-1)!.counts).toStrictEqual(extractReport.counts)
		await expectExtracted(await state.getDir(folder.item.uuid), source)
	})
}

test("an encrypted archive needs its password: none and a wrong one fail before anything is created", async () => {
	const parent = await state.createDir(testDir, "archive-password")
	const source = await archiveSource(parent, 11)
	const format: CompressFormat = { type: "zip", method: { type: "deflate", level: 1 }, encryption: "aes128" }
	// an encrypted format refuses a call without a password before anything runs
	await expect(state.compressItems({ items: [source.root], destination: parent, name: "locked.zip", format })).rejects.toMatchObject({
		kind: "ArchivePasswordRequired"
	})
	// a single compressed file of a folder, before anything is listed
	await expect(
		state.compressItems({
			items: [source.root],
			destination: parent,
			name: "folder.gz",
			format: { type: "single", compression: { codec: "gzip" } }
		})
	).rejects.toMatchObject({ kind: "InvalidState" })
	// and an unencrypted one a call with a password
	await expect(
		state.compressItems({ items: [source.root], destination: parent, name: "open.tar.gz", format: tarGz }, "a password")
	).rejects.toMatchObject({ kind: "InvalidState" })
	const { archive } = await state.compressItems({ items: [source.root], destination: parent, name: "locked.zip", format }, "right horse")

	const into = await state.createDir(parent, "into")
	const missing = await state.extractArchive({ archive: archive!, destination: into, root: { type: "destination" } })
	expect(missing.error?.kind).toBe("ArchivePasswordRequired")
	const wrong = await state.extractArchive({ archive: archive!, destination: into, root: { type: "destination" } }, "wrong horse")
	expect(wrong.error?.kind).toBe("ArchiveWrongPassword")
	for (const refused of [missing, wrong]) {
		expect(refused.topLevel).toHaveLength(0)
		expect(refused.counts.dirsCreated + refused.counts.filesDone).toBe(0n)
	}
	expect(await state.listDir(into)).toMatchObject({ dirs: [], files: [] })

	const right = await state.extractArchive({ archive: archive!, destination: into, root: { type: "destination" } }, "right horse")
	expect(right.error).toBeUndefined()
	await expectExtracted(into, source)
})

test("listArchive lists an archive's entries and checks its password", async () => {
	const parent = await state.createDir(testDir, "archive-list")
	const source = await archiveSource(parent, 12)
	const format: CompressFormat = { type: "zip", method: { type: "bzip2", level: 9 }, encryption: "aes192" }
	const { archive } = await state.compressItems({ items: [source.root], destination: parent, name: "listed.zip", format }, "list me")

	// a zip's index is readable without the password, which is checked only when given; the
	// expansion limit takes plain numbers as well as bigints
	const required = await state.listArchive({ archive: archive!, expansionLimit: { ratio: 1000, floor: 256 * 1024 * 1024 } })
	expect(required.error).toBeUndefined()
	expect([required.format, required.password]).toStrictEqual([{ type: "zip" }, "required"])
	expect((await state.listArchive({ archive: archive! }, "not me")).password).toBe("wrong")

	const batches: ArchiveEntry[][] = []
	const updates: ListUpdate[] = []
	const listed = callbackLog()
	const listing = await state.listArchive(
		{
			archive: archive!,
			onEntriesBatch: entries => {
				listed.note("entries")
				batches.push(entries)
			},
			onUpdate: update => {
				listed.note("update")
				updates.push(update)
			}
		},
		"list me"
	)
	expect(await listed.resolved()).toBe(0)
	expect(listing.password).toBe("right")
	expect([listing.omittedEntries, listing.undeliveredEntries]).toStrictEqual([0n, 0n])
	// every entry the listing keeps came through the callback, before the update counting it
	expect(batches.flat()).toStrictEqual(listing.entries)
	expect(listed.log.indexOf("entries")).toBeLessThan(listed.log.lastIndexOf("update"))
	expect(updates.at(-1)).toMatchObject({ phase: "done", entries: BigInt(listing.entries.length) })
	const files = listing.entries.filter(entry => entry.kind.type === "file").sort((a, b) => a.storedPath.localeCompare(b.storedPath))
	expect(files.map(entry => [entry.path?.path, entry.size, entry.encrypted, entry.skip])).toStrictEqual([
		["archived/notes.txt", BigInt(source.notes.length), true, undefined],
		["archived/sub/data.bin", BigInt(source.data.length), true, undefined]
	])
	expect(listing.totals).toMatchObject({ files: 2n, bytes: BigInt(source.notes.length + source.data.length), skipped: 0n })
	expect(listing.entries.every(entry => entry.id.archive === archive!.uuid)).toBe(true)
})

test("extractArchiveEntries extracts the entries chosen below a base, one again into a retry's directory, and refuses a bad call", async () => {
	const parent = await state.createDir(testDir, "archive-entries")
	const source = await archiveSource(parent, 13)
	const { archive } = await state.compressItems({ items: [source.root], destination: parent, name: "chosen.tar.gz", format: tarGz })
	const { entries } = await state.listArchive({ archive: archive! })
	const entry = (path: string) => entries.find(e => e.path?.path === path)!

	// the chosen directory and what is below it, at its path less the base
	const into = await state.createDir(parent, "into")
	const report = await state.extractArchiveEntries({
		archive: archive!,
		entries: [entry("archived/sub").id],
		base: "archived",
		destination: into,
		root: { type: "destination" }
	})
	expect(report.error).toBeUndefined()
	const { dirs, files } = await state.listDir(into)
	expect([dirs.map(nameOf), files]).toStrictEqual([["sub"], []])
	expect(await state.downloadFile(await childFile(dirs[0], "data.bin"))).toStrictEqual(source.data)

	// a retry as a failure carries it (no entry can be made to fail against the server): the
	// directory nearest the entry that the extract created, and that directory's path in the
	// archive. The entry lands there again, beside what is in its way
	const retry: ExtractRetry = { destination: dirs[0].uuid, destinationDir: dirs[0], base: "archived/sub" }
	const again = await state.extractArchiveEntries({
		archive: archive!,
		entries: [entry("archived/sub/data.bin").id],
		base: retry.base,
		destination: retry.destinationDir,
		root: { type: "destination" }
	})
	expect(again.error).toBeUndefined()
	expect(again.renamed.map(renamed => [renamed.name, renamed.reason])).toStrictEqual([["data (1).bin", "duplicateName"]])

	// a tar's entry not below the base fails the extract once it is reached, with its report
	const outside = await state.extractArchiveEntries({
		archive: archive!,
		entries: [entry("archived/notes.txt").id],
		base: "archived/sub",
		destination: into,
		root: { type: "destination" }
	})
	expect(outside.error).toBeDefined()
	expect(outside.counts.filesDone).toBe(0n)

	// no entry, or a base that is not drive names, is refused before anything runs
	const call = { archive: archive!, destination: into, root: { type: "destination" } } as const
	await expect(state.extractArchiveEntries({ ...call, entries: [], base: "" })).rejects.toMatchObject({ kind: "InvalidState" })
	await expect(
		state.extractArchiveEntries({ ...call, entries: [entry("archived/notes.txt").id], base: "archived/\u0000" })
	).rejects.toMatchObject({ kind: "InvalidName" })
})

/// A USTAR tar of `members`, in order: a file holding `data`, or a hard link naming an earlier
/// member at `linkTo`.
function ustarOf(members: ({ path: string; data: Uint8Array } | { path: string; linkTo: string })[]) {
	const encoder = new TextEncoder()
	const blocks: Uint8Array[] = []
	for (const member of members) {
		const data = "data" in member ? member.data : new Uint8Array(0)
		const header = new Uint8Array(512)
		const put = (at: number, text: string) => header.set(encoder.encode(text), at)
		put(0, member.path)
		put(100, "0000644\0")
		put(108, "0000000\0")
		put(116, "0000000\0")
		put(124, `${data.length.toString(8).padStart(11, "0")}\0`)
		put(136, "00000000000\0")
		// the checksum is taken with its own field as spaces
		put(148, "        ")
		put(156, "data" in member ? "0" : "1")
		if ("linkTo" in member) {
			put(157, member.linkTo)
		}
		put(257, "ustar\0")
		put(263, "00")
		const checksum = header.reduce((sum, byte) => sum + byte, 0)
		put(148, `${checksum.toString(8).padStart(6, "0")}\0 `)
		blocks.push(header)
		const body = new Uint8Array(Math.ceil(data.length / 512) * 512)
		body.set(data)
		blocks.push(body)
	}
	// the end-of-archive marker
	blocks.push(new Uint8Array(1024))
	const tar = new Uint8Array(blocks.reduce((len, block) => len + block.length, 0))
	let at = 0
	for (const block of blocks) {
		tar.set(block, at)
		at += block.length
	}
	return tar
}

test("a listing names the entry a tar's hard link copies, which extracting the link takes along", async () => {
	const parent = await state.createDir(testDir, "archive-hardlink")
	const alpha = new TextEncoder().encode("alpha")
	const tar = ustarOf([
		{ path: "a.txt", data: alpha },
		{ path: "other.txt", data: new TextEncoder().encode("other") },
		{ path: "b", linkTo: "a.txt" },
		{ path: "c", linkTo: "b" }
	])
	const archive = await state.uploadFile(tar, { parent, name: "linked.tar" })
	const { entries } = await state.listArchive({ archive })
	const entry = (path: string) => entries.find(e => e.path?.path === path)!
	// each names the entry it copies, a link naming a link
	expect(entry("b").kind).toStrictEqual({ type: "hardlink", target: "a.txt", targetId: entry("a.txt").id })
	expect(entry("c").kind).toStrictEqual({ type: "hardlink", target: "b", targetId: entry("b").id })

	// alone, a link has nothing to copy; with the entries its listing names, it is extracted
	const call = { archive, base: "", root: { type: "destination" } } as const
	const alone = await state.createDir(parent, "alone")
	const skipped = await state.extractArchiveEntries({ ...call, entries: [entry("c").id], destination: alone })
	expect(skipped.error).toBeUndefined()
	expect(skipped.skipped.map(entry => entry.path)).toStrictEqual(["c"])
	const chain = await state.createDir(parent, "chain")
	const extracted = await state.extractArchiveEntries({
		...call,
		entries: [entry("c").id, entry("b").id, entry("a.txt").id],
		destination: chain
	})
	expect(extracted.error).toBeUndefined()
	const { files } = await state.listDir(chain)
	expect(files.map(nameOf).sort()).toStrictEqual(["a.txt", "b", "c"])
	for (const file of files) {
		expect(await state.downloadFile(file)).toStrictEqual(alpha)
	}
})

test("a wrong password that shows only once entries are read trashes the folder it created, and says so", async () => {
	const parent = await state.createDir(testDir, "archive-late-password")
	const source = await state.createDir(parent, "zeros")
	// past what is read in full to check the password up front: it shows once the entry is read
	await state.uploadFile(new Uint8Array(17 * 1024 * 1024), { parent: source, name: "zeros.bin" })
	const format: CompressFormat = { type: "sevenZ", method: { type: "lzma2", level: 1 }, solid: true, encryption: "entries" }
	const { archive } = await state.compressItems({ items: [source], destination: parent, name: "late.7z", format }, "right")

	const into = await state.createDir(parent, "into")
	const created: ExtractedTopLevelItem[] = []
	const updates: ExtractUpdate[] = []
	const report = await state.extractArchive(
		{
			archive: archive!,
			destination: into,
			root: { type: "newFolder" },
			onTopLevelBatch: items => created.push(...items),
			onUpdate: update => updates.push(update)
		},
		"wrong"
	)
	expect(report.error?.kind).toBe("ArchiveWrongPassword")
	expect(report.counts.filesDone).toBe(0n)
	// the callback got the new folder, which went to the trash, so a retry starts clean: the
	// report no longer lists it, and an update tells which it was
	expect(created.map(item => item.key)).toStrictEqual([{ type: "root" }])
	expect(report.topLevel).toHaveLength(0)
	const trashed = updates.flatMap(update => update.events).filter(event => event.type === "topLevelTrashed")
	expect(trashed.map(event => event.destUuid)).toStrictEqual([created[0].item.uuid])
	expect(await state.listDir(into)).toMatchObject({ dirs: [], files: [] })
})

test("compressItems and extractArchive pause, resume and cancel through managedFuture", async () => {
	const parent = await state.createDir(testDir, "archive-controls")
	const source = await state.createDir(parent, "big")
	for (let i = 0; i < 4; i++) {
		await state.uploadFile(fixtureBytes(2 * 1024 * 1024, 20 + i), { parent: source, name: `part-${i}.bin` })
	}
	const format: CompressFormat = { type: "tar" }

	// paused once it compresses, then resumed
	const pauseSignal = new PauseSignal()
	let pausedOnce = false
	const updates: CompressUpdate[] = []
	const paused = state.compressItems({
		items: [source],
		destination: parent,
		name: "paused.tar",
		format,
		onUpdate: update => {
			updates.push(update)
			if (update.phase === "compressing" && !pausedOnce) {
				pausedOnce = true
				pauseSignal.pause()
			}
		},
		managedFuture: { pauseSignal }
	})
	await vi.waitFor(
		() => {
			expect(updates.some(update => update.runState === "paused")).toBe(true)
		},
		{ timeout: cap(60_000), interval: 100 }
	)
	pauseSignal.resume()
	const resumed = await paused
	expect(resumed.error).toBeUndefined()
	expect(resumed.counts.filesDone).toBe(4n)
	// a paused job reads nothing: every update while it was paused counts the same bytes
	const whilePaused = updates.filter(update => update.runState === "paused")
	expect(new Set(whilePaused.map(update => update.counts.bytesRead)).size).toBe(1)

	// aborted while it compresses: nothing is left behind, and the report says why it ended
	const controller = new AbortController()
	const cancelled = await state.compressItems({
		items: [source],
		destination: parent,
		name: "cancelled.tar",
		format,
		onUpdate: update => {
			if (update.phase === "compressing") {
				controller.abort()
			}
		},
		managedFuture: { abortSignal: controller.signal }
	})
	// the SDK error itself, as the calls throw it
	expect(cancelled.error).toBeInstanceOf(FilenSdkError)
	expect(cancelled.error?.kind).toBe("Cancelled")
	expect(cancelled.archive).toBeUndefined()
	expect((await state.listDir(parent)).files.map(nameOf)).toStrictEqual(["paused.tar"])

	// an extract aborted once its folder exists keeps what it created, and counts the rest
	const extractController = new AbortController()
	const extractUpdates: ExtractUpdate[] = []
	const stopped = await state.extractArchive({
		archive: resumed.archive!,
		destination: parent,
		root: { type: "newFolder", name: "stopped" },
		onTopLevelBatch: () => extractController.abort(),
		onUpdate: update => extractUpdates.push(update),
		managedFuture: { abortSignal: extractController.signal }
	})
	expect(stopped.error).toBeInstanceOf(FilenSdkError)
	expect(stopped.error?.kind).toBe("Cancelled")
	expect(stopped.topLevel.map(item => nameOf(item.item))).toStrictEqual(["stopped"])
	expect(extractUpdates.at(-1)!.phase).toBe("cancelled")
	expect(extractUpdates.at(-1)!.counts).toStrictEqual(stopped.counts)
})

test("sources go to the trash once their archive is verified, an extracted archive for good", async () => {
	const parent = await state.createDir(testDir, "archive-dispose")
	const source = await archiveSource(parent, 14)
	const updates: CompressUpdate[] = []
	const report = await state.compressItems({
		items: [source.root],
		destination: parent,
		name: "disposed.tar.gz",
		format: tarGz,
		dispose: "trash",
		onUpdate: update => updates.push(update)
	})
	expect(report.error).toBeUndefined()
	const trashed = { uuid: source.root.uuid, outcome: { type: "disposed", how: "trash", bytesFreed: 0n } }
	expect(report.dispositions).toStrictEqual([trashed])
	expect(updates.flatMap(update => update.events).filter(event => event.type === "sourceDisposition")).toStrictEqual([
		{ type: "sourceDisposition", ...trashed }
	])
	// trashing reads nothing back: the sources can be restored
	expect(report.counts.bytesVerified).toBe(0n)
	expect((await state.getDir(source.root.uuid)).parent).toBe("trash")

	// the archive, extracted and verified, is deleted for good
	const archive = report.archive!
	const extracted = await state.extractArchive({
		archive,
		destination: parent,
		root: { type: "newFolder" },
		dispose: "deletePermanently"
	})
	expect(extracted.error).toBeUndefined()
	expect(extracted.dispositions).toStrictEqual([
		{ uuid: archive.uuid, outcome: { type: "disposed", how: "deletePermanently", bytesFreed: archive.size } }
	])
	await expectExtracted(await state.getDir(extracted.topLevel[0].item.uuid), source)

	// a brotli stream carries no checksum, so nothing confirms what it held and it is kept; a
	// single compressed file ignores newFolder and lands in the destination itself
	const note = await state.uploadFile(new TextEncoder().encode("a note on its own"), { parent, name: "note.txt" })
	const { archive: single } = await state.compressItems({
		items: [note],
		destination: parent,
		name: "note.txt.br",
		format: { type: "single", compression: { codec: "brotli" } }
	})
	expect(archiveFormatOfName("note.txt.br")).toStrictEqual({ type: "single", codec: "brotli" })
	const kept = await state.extractArchive({
		archive: single!,
		destination: parent,
		root: { type: "newFolder", name: "Notes" },
		dispose: "trash"
	})
	expect(kept.error).toBeUndefined()
	expect(kept.dispositions).toStrictEqual([
		{ uuid: single!.uuid, outcome: { type: "kept", reason: { type: "unconfirmed" }, bytesFreed: 0n } }
	])
	expect(kept.topLevel.map(item => [item.item.type, nameOf(item.item)])).toStrictEqual([["file", "note (1).txt"]])
})

test("extractArchive reads an archive from a public link, called without a single callback", async () => {
	const parent = await state.createDir(testDir, "archive-link")
	const source = await archiveSource(parent, 15)
	const { archive } = await state.compressItems({
		items: [source.root],
		destination: parent,
		name: "linked.tar.xz",
		format: { type: "tar", compression: { codec: "xz", level: 1 } }
	})
	const link = await state.publicLinkFile(archive!)
	const linked = await unauthClient.getLinkedFile(link.linkUuid, getFileMeta(archive!.meta)!.key, null)

	const listing = await state.listArchive({ archive: linked })
	expect(listing.format).toStrictEqual({ type: "tar", codec: "xz" })
	const report = await state.extractArchive({ archive: linked, destination: parent, root: { type: "newFolder", name: "from-link" } })
	expect(report.error).toBeUndefined()
	expect(nameOf(report.topLevel[0].item)).toBe("from-link")
	await expectExtracted(await state.getDir(report.topLevel[0].item.uuid), source)
	// a linked archive is not the user's to remove
	await expect(
		state.extractArchive({ archive: linked, destination: parent, root: { type: "destination" }, dispose: "trash" })
	).rejects.toMatchObject({ kind: "InvalidState" })
})

test("the archive helpers name formats, their levels, what fits and what a name holds", () => {
	const zip: CompressFormat = { type: "zip", method: { type: "deflate", level: 6 }, encryption: "aes256" }
	expect(archiveExtension(zip)).toBe(".zip")
	expect(archiveExtension({ type: "tar", compression: { codec: "zstd" } })).toBe(".tar.zst")
	expect(archiveExtension({ type: "single", compression: { codec: "gzip" } })).toBe(".gz")
	expect(archiveFormatLevels(zip)).toStrictEqual({ min: 1, max: 9, defaultLevel: 6 })
	expect(archiveFormatLevels({ type: "tar", compression: { codec: "zstd" } })).toStrictEqual({ min: 1, max: 1, defaultLevel: 1 })
	expect(archiveFormatLevels({ type: "sevenZ", method: { type: "ppmd", level: 9 }, solid: false })?.defaultLevel).toBe(5)
	expect(archiveFormatLevels({ type: "tar" })).toBeUndefined()
	expect(archiveEncoderMemory({ type: "tar" })).toBe(0n)
	// a level the format does not take is refused
	expect(() => archiveEncoderMemory({ type: "zip", method: { type: "deflate", level: 10 } })).toThrow()

	// the web's default budget runs LZMA2 up to level 6 (level 7 needs about 193 MiB)
	const budget = state.archiveCodecMemBudget()
	expect(budget).toBe(128n << 20n)
	const lzma2 = (level: number): CompressFormat => ({ type: "sevenZ", method: { type: "lzma2", level }, solid: true })
	const max = archiveMaxLevel(lzma2(9), budget)
	expect(max).toBe(6)
	expect(archiveEncoderMemory(lzma2(max!))).toBeLessThanOrEqual(budget)
	expect(archiveEncoderMemory(lzma2(max! + 1))).toBeGreaterThan(budget)

	expect(archiveDefaultName("photos.tar.gz")).toBe("photos")
	expect(archiveDefaultName("photos.TGZ")).toBe("photos")
	expect(archiveFormatOfName("photos.tgz")).toStrictEqual({ type: "tar", codec: "gzip" })
	expect(archiveFormatOfName("photos.tar")?.type).toBe("tar")
	expect(archiveFormatOfName("photos.7z")).toStrictEqual({ type: "sevenZ" })
	expect(archiveFormatOfName("notes.txt")).toBeUndefined()
})

afterAll(async () => {
	if (state && testDir) {
		await state.deleteDirPermanently(testDir)
	}
})

export function jsonBigIntReplacer(_: string, value: unknown) {
	if (typeof value === "bigint") {
		return `$bigint:${value.toString()}n`
	}

	return value
}
