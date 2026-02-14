import init, {
	initThreadPool,
	login,
	Client,
	fromStringified,
	type Dir,
	type File,
	PauseSignal,
	FilenSdkError,
	ListenerHandle,
	type SocketEvent,
	type FileMeta,
	type DecryptedFileMeta,
	type DecryptedDirMeta,
	type DirMeta
} from "./sdk-rs.js"
import { expect, beforeAll, test, afterAll, afterEach } from "vitest"
import { ZipReader, type Entry } from "@zip.js/zip.js"

console.log("Initializing WASM...")
await init()
const threads = Math.max((navigator.hardwareConcurrency || 5) - 1, 1)
console.log(`WASM initialized ${threads} threads`)
const now = Date.now()
await initThreadPool(threads)
console.log(`WASM initialized ${threads} in ${Date.now() - now}ms`)

let state: Client
let shareClient: Client
let testDir: Dir
const allEvents: SocketEvent[] = []
const listenerHandles: ListenerHandle[] = []
// let _shareTestDir: Dir
const listenerErrors: Error[] = []

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

beforeAll(async () => {
	await Promise.all([
		(async () => {
			if (!import.meta.env.VITE_TEST_EMAIL) {
				throw new Error("VITE_TEST_EMAIL environment variable is not set")
			}
			if (!import.meta.env.VITE_TEST_PASSWORD) {
				throw new Error("VITE_TEST_PASSWORD environment variable is not set")
			}
			state = await login({
				email: import.meta.env.VITE_TEST_EMAIL,
				password: import.meta.env.VITE_TEST_PASSWORD
			})

			console.log("logged in, setting up socket listener")
			listenerHandles.push(
				await state.addEventListener(event => {
					if (!assertNoMaps(event)) {
						listenerErrors.push(new Error("Socket event contained a Map", { cause: event }))
					}
					allEvents.push(event)
				}, null)
			)

			const maybeDir = await state.findItemInDir(state.root(), "wasm-test-dir")
			if (maybeDir) {
				if (maybeDir.type === "dir") {
					await state.deleteDirPermanently(maybeDir)
				} else {
					throw new Error("Expected testDir to be a Dir, but it was a File")
				}
			}
			testDir = await state.createDir(state.root(), "wasm-test-dir")
		})(),
		(async () => {
			if (!import.meta.env.VITE_TEST_SHARE_EMAIL) {
				throw new Error("VITE_TEST_SHARE_EMAIL environment variable is not set")
			}
			if (!import.meta.env.VITE_TEST_SHARE_PASSWORD) {
				throw new Error("VITE_TEST_SHARE_PASSWORD environment variable is not set")
			}
			shareClient = await login({ email: import.meta.env.VITE_TEST_SHARE_EMAIL, password: import.meta.env.VITE_TEST_SHARE_PASSWORD })
		})()
	])
}, 120000)

afterEach(() => {
	if (listenerErrors.length > 0) {
		const errors = [...listenerErrors]
		listenerErrors.length = 0
		console.error("Socket listener errors detected:", errors[0].cause)
		throw errors
	}
})

function getFileMeta(meta: FileMeta): DecryptedFileMeta | null {
	if (meta.type === "Decoded") {
		return meta
	} else {
		return null
	}
}

function getDirMeta(meta: DirMeta): DecryptedDirMeta | null {
	if (meta.type === "Decoded") {
		return meta
	} else {
		return null
	}
}

test("login", async () => {
	expect(state).toBeDefined()
	expect(state.root().uuid).toBeDefined()
})

test("serialization", async () => {
	const serializedState = await state.toStringified()
	expect(serializedState.rootUuid).toEqual(state.root().uuid)
	const newState = fromStringified(serializedState)
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
		knownSize: BigInt(data.length)
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

test("Zip Download", async () => {
	const dirA = await state.createDir(testDir, "a")
	const dirB = await state.createDir(dirA, "b")
	await state.createDir(testDir, "c")

	const file = await state.uploadFile(new TextEncoder().encode("root file content"), {
		parent: testDir,
		name: "file.txt"
	})
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

	const { readable, writable } = new TransformStream()

	// we don't await here because TransformStream doesn't have a buffer
	// so this would hang forever
	state.downloadItemsToZip({
		items: [file, dirA],
		writer: writable,
		progress: (_bytesWritten, _totalBytes, _itemsProcessed, _totalItems) => {
			//
		}
	})

	const zipReader = new ZipReader(readable)

	const entries = await zipReader.getEntries()
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
		const { readable, writable } = new TransformStream()
		// we don't await here because TransformStream doesn't have a buffer
		// so this would hang forever
		entry.getData!(writable)
		expect(await streamToUint8Array(readable)).toEqual(expected)
	}

	await compareFileToEntry(map.get("file.txt")!, new TextEncoder().encode("root file content"), file)
	await compareFileToEntry(map.get("a/file1.txt")!, new TextEncoder().encode("file 1 content"), file1)
	await compareFileToEntry(map.get("a/b/file2.txt")!, new TextEncoder().encode("file 2 content"), file2)
	await compareFileToEntry(map.get("a/b/file3.txt")!, new TextEncoder().encode("file 3 content"), file3)
})

test("sharing", async () => {
	const dir = await state.createDir(testDir, "share-test-dir")
	const file = await state.uploadFile(new TextEncoder().encode("shared file content"), {
		parent: dir,
		name: "shared-file.txt"
	})

	const contacts = await state.getContacts()
	let contact
	for (const c of contacts) {
		if (c.email === import.meta.env.VITE_TEST_SHARE_EMAIL) {
			contact = c
			break
		}
	}
	if (!contact) {
		const reqUuid = await state.sendContactRequest(import.meta.env.VITE_TEST_SHARE_EMAIL!)
		const reqs = await shareClient.listIncomingContactRequests()
		const req = reqs.find(r => r.uuid === reqUuid)
		if (!req) {
			throw new Error("Contact request not found")
		}
		await shareClient.acceptContactRequest(req.uuid)
		contact = (await state.getContacts()).find(c => c.email === import.meta.env.VITE_TEST_SHARE_EMAIL!)!
	}
	expect(contact).toBeDefined()
	await state.shareDir(dir, contact, (downloaded: number, total: number | undefined) => {
		console.log(`Shared dir upload progress: ${downloaded}/${total}`)
	})
	const shared = await state.listOutShared(null, contact)
	const sharedDir = shared.dirs.find(d => d.dir.uuid === dir.uuid)
	expect(sharedDir).toBeDefined()
	expect(sharedDir?.dir?.uuid).toEqual(dir.uuid)

	await shareClient.listInShared()
	const sharedDirs = (await shareClient.listInShared()).dirs
	const sharedDirIn = sharedDirs.find(d => d.dir.uuid === dir.uuid)
	expect(sharedDirIn).toBeDefined()

	const files = (await shareClient.listInShared(sharedDirIn?.dir)).files
	expect(files.find(f => f.file.uuid === file.uuid)).toBeDefined()

	await state.deleteContact(contact.uuid)
})

test("block", async () => {
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
	expect(requestsAfter.length).toBe(0)

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

			const thumb = await state.makeThumbnailInMemory({
				file: file,
				maxHeight: 100,
				maxWidth: 100
			})

			expect(thumb).toBeDefined()

			const blob = new Blob([thumb!.webpData], { type: "image/webp" })
			const bitmap = await createImageBitmap(blob)

			expect(bitmap.width).toBeLessThanOrEqual(100)
			expect(bitmap.height).toBeLessThanOrEqual(100)

			expect(blob.type).toBe("image/webp")

			// Clean up
			bitmap.close()

			completed.push(ext)
		})
	)

	// avif and heic do not currently work
	expect(completed).not.toContainEqual("avif")
	expect(completed).toContainEqual("gif")
	expect(completed).not.toContainEqual("heif")
	expect(completed).toContainEqual("jpg")
	expect(completed).toContainEqual("png")
	expect(completed).toContainEqual("tiff")
	expect(completed).toContainEqual("qoi")
	expect(completed).toContainEqual("webp")
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

	updatedFile = (await state.setFavorite(updatedFile, true)) as File
	updatedDir = (await state.setFavorite(updatedDir, true)) as Dir
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
	let chat = await state.createChat([])
	expect(chat).toBeDefined()
	chat = await state.renameChat(chat, "Test Chat")
	expect(chat.name).toBe("Test Chat")

	chat = await state.sendChatMessage(chat, "This is a test message")
	expect(chat.lastMessage?.message).toEqual("This is a test message")
	const fetchedChat = await state.getChat(chat.uuid)
	expect(fetchedChat).toEqual(chat)

	// sleep for 5s
	await new Promise(resolve => setTimeout(resolve, 5000))

	const chatEvent = allEvents.find(e => e.type === "chatMessageNew" && e.msg.chat === chat.uuid)

	expect(chatEvent).toBeDefined()

	if (chatEvent?.type !== "chatMessageNew") {
		throw new Error("Expected chatMessageNew event")
	}

	expect(chatEvent.msg).toEqual(fetchedChat?.lastMessage)
})

test("search", async () => {
	const dir = await state.createDir(testDir, "search-dir-124asdfas;dlkfj")
	const file = await state.uploadFile(new TextEncoder().encode("search file content"), {
		parent: dir,
		name: "search-file-124asdfas;dlkfj.txt"
	})

	const results = await state.findItemMatchesForName("124asdfas;dlkfj")
	expect(results.find(i => i.item.uuid === dir.uuid)).toBeDefined()
	expect(results.find(i => i.item.uuid === file.uuid)).toBeDefined()
})

test("authError", async () => {
	const badStringified = await state.toStringified()
	badStringified.apiKey = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
	const badState = fromStringified(badStringified)
	try {
		await badState.listDir(badState.root())
		expect.fail("Expected error to be thrown")
	} catch (e) {
		expect(e).toBeInstanceOf(FilenSdkError)
		expect((e as FilenSdkError).kind).toEqual("Unauthenticated")
		expect((e as FilenSdkError).toString()).toContain("v3/dir/content")
	}

	let gotAuthFailedEvent = false
	try {
		await badState.addEventListener(
			event => {
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
		expect(gotAuthFailedEvent).toBe(true)
	}
})

test("sockets", async () => {
	expect(await state.isSocketConnected()).toBe(true)
	for (const handle of listenerHandles) {
		handle.free()
	}
	expect(await state.isSocketConnected()).toBe(false)
	{
		/* eslint-disable @typescript-eslint/no-unused-vars */
		using _ = await state.addEventListener(() => {}, null)
		expect(await state.isSocketConnected()).toBe(true)
	}
	expect(await state.isSocketConnected()).toBe(false)
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

		const initRes = await fetch(`/serviceWorker/init?stringifiedClient=${encodeURIComponent(jsonClient)}`)

		expect(initRes.ok).toBe(true)

		// wait a bit to ensure service worker is ready and client is loaded
		await new Promise<void>(resolve => setTimeout(resolve, 5000))

		const file = await state.uploadFile(new TextEncoder().encode("service worker file content"), {
			parent: testDir,
			name: "sw-file.txt"
		})

		const stringifiedFile = JSON.stringify(file, jsonBigIntReplacer)

		const res = await fetch("/serviceWorker/download?file=" + encodeURIComponent(stringifiedFile))
		expect(res.ok).toBe(true)
		const text = await res.text()
		expect(text).toBe("service worker file content")
	} finally {
		clearInterval(intervalId)
	}
})

afterAll(async () => {
	if (state && testDir) {
		await state.deleteDirPermanently(testDir)
	}
})

async function streamToUint8Array(readableStream: ReadableStream<Uint8Array>): Promise<Uint8Array> {
	const chunks = []
	const reader = readableStream.getReader()

	while (true) {
		const { done, value } = await reader.read()
		if (done) break
		chunks.push(value)
	}

	// Concatenate all chunks
	const totalLength = chunks.reduce((sum, chunk) => sum + chunk.length, 0)
	const result = new Uint8Array(totalLength)
	let offset = 0
	for (const chunk of chunks) {
		result.set(chunk, offset)
		offset += chunk.length
	}

	return result
}

export function jsonBigIntReplacer(_: string, value: unknown) {
	if (typeof value === "bigint") {
		return `$bigint:${value.toString()}n`
	}

	return value
}
