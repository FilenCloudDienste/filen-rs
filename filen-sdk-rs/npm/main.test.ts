import { login, Client, fromStringified, type Dir } from "./browser/sdk-rs.js"
import { expect, beforeAll, test, afterAll } from "vitest"
import "dotenv/config"

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

afterAll(async () => {
	await state?.deleteDirPermanently(testDir)
})
