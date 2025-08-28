import { login, Client, fromStringified, type Dir, type File } from "./browser/sdk-rs.js"
import { expect, beforeAll, test, afterAll } from "vitest"
import { tmpdir } from "os"
import { createWriteStream, openAsBlob } from "fs"
import "dotenv/config"
import Stream from "stream"
import { ZipReader, type Entry } from "@zip.js/zip.js"

let state: Client
let testDir: Dir
beforeAll(async () => {
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
})

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
		known_size: BigInt(data.length)
	})

	expect(progress).toBe(BigInt(data.length))

	let buffer = new ArrayBuffer(0)
	const webStream = new WritableStream({
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
})

test("abort", async () => {
	const abortController = new AbortController()
	const fileAPromise = state.uploadFile(Buffer.from("file a"), {
		name: "abort a.txt",
		parent: testDir,
		abortSignal: abortController.signal
	})

	const fileBPromise = state.uploadFile(Buffer.from("file b"), {
		name: "abort b.txt",
		parent: testDir
	})

	const abortControllerDelayed = new AbortController()

	const fileCPromise = state.uploadFile(Buffer.from("file c"), {
		name: "abort c.txt",
		parent: testDir,
		abortSignal: abortControllerDelayed.signal
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
		writer: webStream
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
