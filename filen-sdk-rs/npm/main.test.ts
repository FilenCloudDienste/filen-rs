import { login, Client, fromStringified, type Dir, type File, PauseSignal } from "./bundler/sdk-rs.js"
import { expect, beforeAll, test, afterAll } from "vitest"
import { tmpdir } from "os"
import { createWriteStream, openAsBlob } from "fs"
import "dotenv/config"
import Stream from "stream"
import { ZipReader, type Entry } from "@zip.js/zip.js"
import sharp from "sharp"

let state: Client
let shareClient: Client
let testDir: Dir
// let _shareTestDir: Dir

beforeAll(async () => {
	await Promise.all([
		(async () => {
			const email = process.env.TEST_EMAIL
			if (!email) {
				throw new Error("TEST_EMAIL environment variable is not set")
			}
			const password = process.env.TEST_PASSWORD
			if (!password) {
				throw new Error("TEST_PASSWORD environment variable is not set")
			}
			state = await login(email, password)

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
			const email = process.env.TEST_SHARE_EMAIL
			if (!email) {
				throw new Error("TEST_SHARE_EMAIL environment variable is not set")
			}
			const password = process.env.TEST_SHARE_PASSWORD
			if (!password) {
				throw new Error("TEST_SHARE_PASSWORD environment variable is not set")
			}
			shareClient = await login(email, password)
		})()
	])
}, 120000)

test("login", async () => {
	expect(state).toBeDefined()
	expect(state.root().uuid).toBeDefined()
})

test("serialization", async () => {
	const serializedState = state.toStringified()
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
	expect(resp.length).toBe(2)
	expect(resp[0]).toBeInstanceOf(Array)
	expect(resp[1]).toBeInstanceOf(Array)
})

test("Directory", async () => {
	const before = new Date().getTime()
	let dir = await state.createDir(testDir, "test-dir")
	const [dirs, files] = await state.listDir(dir)
	expect(dirs.length).toBe(0)
	expect(files.length).toBe(0)

	const after = new Date().getTime()
	expect(dir).toBeDefined()
	expect(dir.uuid).toBeDefined()
	expect(dir.parent).toBe(testDir.uuid)
	expect(dir.meta?.name).toBe("test-dir")
	expect(dir.meta?.created).toBeGreaterThanOrEqual(before)
	expect(dir.meta?.created).toBeLessThanOrEqual(after)
	dir = await state.trashDir(dir)
	expect(dir.parent).toBe("trash")
	await state.deleteDirPermanently(dir)
})

test("File", async () => {
	const created = BigInt(new Date().getTime())
	const before = BigInt(new Date().getTime())
	let file = await state.uploadFile(Buffer.from("test-file.txt"), {
		parent: testDir,
		name: "test-file.txt",
		created: created
	})
	const after = new Date().getTime()
	expect(file).toBeDefined()
	expect(file.uuid).toBeDefined()
	expect(file.parent).toBe(testDir.uuid)
	expect(file.meta?.name).toBe("test-file.txt")
	expect(file.meta?.created).toStrictEqual(created)
	expect(file.meta?.modified).toBeGreaterThanOrEqual(before)
	expect(file.meta?.modified).toBeLessThanOrEqual(after)
	expect(file.size).toBe(BigInt("test-file.txt".length))
	const data = await state.downloadFile(file)
	expect(new TextDecoder().decode(data)).toBe("test-file.txt")
	file = await state.trashFile(file)
	expect(file.parent).toBe("trash")
	await state.deleteFilePermanently(file)
})

test("File Streams", async () => {
	// Write test file
	const data = "test file data"

	// Create a readable stream from the file
	const blob = new Blob(["test file data"])

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

	let buffer = new ArrayBuffer(0)
	let webStream = new WritableStream({
		write(chunk) {
			const bytes = chunk instanceof Uint8Array ? chunk : new TextEncoder().encode(chunk)

			// Create new buffer with combined size
			const newBuffer = new ArrayBuffer(buffer.byteLength + bytes.length)
			const newView = new Uint8Array(newBuffer)

			// Copy existing data
			newView.set(new Uint8Array(buffer))
			// Append new data
			newView.set(bytes, buffer.byteLength)

			buffer = newBuffer
		}
	})
	let downloadProgress = 0n
	await state.downloadFileToWriter({
		file: remoteFile,
		writer: webStream,
		progress: bytes => {
			downloadProgress = bytes
		}
	})

	expect(downloadProgress).toBe(BigInt(data.length))

	// Convert both to Uint8Array for comparison
	const downloadedBytes = new Uint8Array(buffer)
	const expectedBytes = new TextEncoder().encode(data)
	expect([...downloadedBytes]).toEqual([...expectedBytes])

	buffer = new ArrayBuffer(0)
	webStream = new WritableStream({
		write(chunk) {
			const bytes = chunk instanceof Uint8Array ? chunk : new TextEncoder().encode(chunk)

			// Create new buffer with combined size
			const newBuffer = new ArrayBuffer(buffer.byteLength + bytes.length)
			const newView = new Uint8Array(newBuffer)

			// Copy existing data
			newView.set(new Uint8Array(buffer))
			// Append new data
			newView.set(bytes, buffer.byteLength)

			buffer = newBuffer
		}
	})
	await state.downloadFileToWriter({
		file: remoteFile,
		writer: webStream,
		progress: bytes => {
			downloadProgress = bytes
		},
		start: BigInt(5),
		end: BigInt(9)
	})
	expect([...new Uint8Array(buffer)]).toEqual([...new TextEncoder().encode("file")])
})

test("abort", async () => {
	const abortController = new AbortController()
	const fileAPromise = state.uploadFile(Buffer.from("file a"), {
		name: "abort a.txt",
		parent: testDir,
		managedFuture: {
			abortSignal: abortController.signal
		}
	})

	const fileBPromise = state.uploadFile(Buffer.from("file b"), {
		name: "abort b.txt",
		parent: testDir
	})

	const abortControllerDelayed = new AbortController()

	const fileCPromise = state.uploadFile(Buffer.from("file c"), {
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

	await expect(fileAPromise).rejects.toThrowError("Operation was cancelled")
	await expect(fileCPromise).rejects.toThrowError("Operation was cancelled")

	const fileB = await fileBPromise
	const [, files] = await state.listDir(testDir)

	expect(files).toContainEqual(fileB)
	for (const file of files) {
		expect(file.meta?.name).not.toBe("abort a.txt")
		expect(file.meta?.name).not.toBe("abort c.txt")
	}
})

test("pause", async () => {
	const pauseSignal = new PauseSignal()
	let fileAPromiseResolved = false
	const fileAPromise = state.uploadFile(Buffer.from("file a"), {
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
	const fileBPromise = state.uploadFile(Buffer.from("file b"), {
		name: "pause b.txt",
		parent: testDir,
		managedFuture: {
			pauseSignal: pauseSignal
		}
	})
	fileBPromise.then(() => {
		fileBPromiseResolved = true
	})

	const fileCPromise = state.uploadFile(Buffer.from("file c"), {
		name: "pause c.txt",
		parent: testDir
	})

	let fileDPromiseResolved = false
	const fileDPromise = state.uploadFile(Buffer.from("file d"), {
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
	expect(fileC.meta?.name).toBe("pause c.txt")
	await new Promise(resolve => setTimeout(resolve, 2000))
	expect(fileAPromiseResolved).toBe(false)
	expect(fileBPromiseResolved).toBe(false)
	expect(fileDPromiseResolved).toBe(true)
	pauseSignal.resume()
	console.log("resumed, awaiting a and b")
	const fileA = await fileAPromise
	console.log("file a done")
	expect(fileA).toBeDefined()
	expect(fileA.meta?.name).toBe("pause a.txt")
	console.log("awaiting b")
	await new Promise(resolve => setTimeout(resolve, 2000))
	console.log("checking b")
	expect(fileBPromiseResolved).toBe(true)
	const fileB = await fileBPromise
	expect(fileB).toBeDefined()
	expect(fileB.meta?.name).toBe("pause b.txt")
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

	const writeStream = createWriteStream(`${tmpdir()}/test-zip-download.zip`)
	const webStream = Stream.Writable.toWeb(writeStream)
	await state.downloadItemsToZip({
		items: [file, dirA],
		writer: webStream,
		progress: (_bytesWritten, _totalBytes, _itemsProcessed, _totalItems) => {
			//
		}
	})
	writeStream.end()

	const zipBlob = await openAsBlob(`${tmpdir()}/test-zip-download.zip`)

	const zipReader = new ZipReader(zipBlob.stream())

	const entries = await zipReader.getEntries()
	const map = new Map<string, Entry>()
	for (const entry of entries) {
		map.set(entry.filename, entry)
	}

	const compareFileToEntry = async (entry: Entry, expected: Uint8Array, expectedFile: File) => {
		// zip.js has bad precision for dates, so we compare in seconds
		expect(BigInt(entry.creationDate!.getTime())).toEqual(expectedFile.meta?.created)
		expect(entry.lastModDate.getTime() / 1000).toEqual(Math.floor(Number(expectedFile.meta?.modified) / 1000))
		expect(BigInt(entry.uncompressedSize)).toEqual(expectedFile.size)
		const object = createMemoryWritableStream()
		await entry.getData!(object.stream)
		expect(object.getBuffer()).toEqual(expected)
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
		if (c.email === process.env.TEST_SHARE_EMAIL) {
			contact = c
			break
		}
	}
	if (!contact) {
		const reqUuid = await state.sendContactRequest(process.env.TEST_SHARE_EMAIL!)
		const reqs = await shareClient.listIncomingContactRequests()
		const req = reqs.find(r => r.uuid === reqUuid)
		if (!req) {
			throw new Error("Contact request not found")
		}
		await shareClient.acceptContactRequest(req.uuid)
		contact = (await state.getContacts()).find(c => c.email === process.env.TEST_SHARE_EMAIL!)!
	}
	expect(contact).toBeDefined()
	await state.shareDir(dir, contact)
	const shared = await state.listOutShared(null, contact)
	const sharedDir = shared[0].find(d => d.dir.uuid === dir.uuid)
	expect(sharedDir).toBeDefined()
	expect(sharedDir.dir.uuid).toEqual(dir.uuid)

	await shareClient.listInShared()
	const sharedDirs = await shareClient.listInShared()
	const sharedDirIn = sharedDirs[0].find(d => d.dir.uuid === dir.uuid)
	expect(sharedDirIn).toBeDefined()
	const [, files] = await shareClient.listInShared(sharedDirIn.dir)
	expect(files.find(f => f.file.uuid === file.uuid)).toBeDefined()

	await state.deleteContact(contact.uuid)
})

test("block", async () => {
	const contacts = await state.getContacts()
	let contact
	for (const c of contacts) {
		if (c.email === process.env.TEST_SHARE_EMAIL) {
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
	await state.sendContactRequest(process.env.TEST_SHARE_EMAIL!)
	const requests = await shareClient.listIncomingContactRequests()
	const req = requests.find(r => r.email === process.env.TEST_EMAIL)
	if (!req) {
		throw new Error("Contact request not found")
	}

	await shareClient.blockContact(req.email)
	const blocked = await shareClient.getBlockedContacts()
	expect(blocked.length).toBe(1)
	expect(blocked[0].email).toBe(process.env.TEST_EMAIL)

	const requestsAfter = await shareClient.listIncomingContactRequests()
	expect(requestsAfter.length).toBe(0)

	await shareClient.unblockContact(blocked[0].uuid)
	const blockedAfter = await shareClient.getBlockedContacts()
	expect(blockedAfter.length).toBe(0)

	const requestsFinal = await shareClient.listIncomingContactRequests()
	expect(requestsFinal.length).toBe(1)
	expect(requestsFinal[0].email).toBe(process.env.TEST_EMAIL)
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
			const parrotImage = await openAsBlob(`test_imgs/${img}.${ext}`)
			const file = await state.uploadFile(await parrotImage.bytes(), {
				parent: testDir,
				name: `${img}.${ext}`
			})

			if (!file.canMakeThumbnail) {
				console.warn(`Skipping thumbnail test for unsupported mime type: ${file.meta?.mime}`)
				return
			}

			const thumb = await state.makeThumbnailInMemory({
				file: file,
				maxHeight: 100,
				maxWidth: 100
			})

			expect(thumb).toBeDefined()

			const metaPromise = sharp(thumb!.webpData).metadata()
			await expect(metaPromise).resolves.toBeDefined()
			const meta = await metaPromise
			expect(meta.width).toBeLessThanOrEqual(100)
			expect(meta.height).toBeLessThanOrEqual(100)
			expect(meta.format).toBe("webp")
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

	expect(file.meta?.name).toBe("meta-file.txt")
	expect(file.meta?.created).toBeDefined()
	expect(file.meta?.modified).toBeDefined()

	let updatedFile = await state.updateFileMetadata(file, {
		created: null
	})
	expect(updatedFile.meta?.created).toBeUndefined()

	updatedFile = await state.updateFileMetadata(file, {
		name: "meta-file-renamed.txt"
	})
	expect(updatedFile.meta?.name).toBe("meta-file-renamed.txt")

	const dir = await state.createDir(testDir, "meta-dir")
	expect(dir.meta?.name).toBe("meta-dir")
	expect(dir.meta?.created).toBeDefined()

	let updatedDir = await state.updateDirMetadata(dir, {
		created: null
	})
	expect(updatedDir.meta?.created).toBeUndefined()

	updatedDir = await state.updateDirMetadata(dir, {
		name: "meta-dir-renamed"
	})
	expect(updatedDir.meta?.name).toBe("meta-dir-renamed")

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
	const [note1, tag1] = await state.addTagToNote(note, tag)
	note = note1
	tag = tag1
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
	expect(chat.lastMessage?.inner.message).toEqual("This is a test message")
	const fetchedChat = await state.getChat(chat.uuid)
	expect(fetchedChat).toEqual(chat)
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

afterAll(async () => {
	if (state && testDir) {
		await state?.deleteDirPermanently(testDir)
	}
})

export function createMemoryWritableStream(): {
	stream: WritableStream<Uint8Array>
	getBuffer: () => Uint8Array
} {
	const chunks: Uint8Array[] = []

	const stream = new WritableStream<Uint8Array>({
		write(chunk) {
			chunks.push(chunk)
		}
	})

	const getBuffer = () => {
		const totalLength = chunks.reduce((sum, chunk) => sum + chunk.length, 0)
		const result = new Uint8Array(totalLength)
		let offset = 0

		for (const chunk of chunks) {
			result.set(chunk, offset)
			offset += chunk.length
		}

		return result
	}

	return {
		stream,
		getBuffer
	}
}
