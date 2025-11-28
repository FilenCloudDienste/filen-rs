/// <reference types="@types/serviceworker" />

import init, { Client, fromStringified, type StringifiedClient, type File } from "./service-worker/sdk-rs.js"
import filenSdkRsWasmPath from "./service-worker/sdk-rs_bg.wasm?url"

self.addEventListener("install", () => {
	console.log("Installing service worker...")

	self.skipWaiting()
		.then(() => {
			console.log("Service worker installed")
		})
		.catch(console.error)
})

self.addEventListener("activate", (event: ExtendableEvent) => {
	event.waitUntil(
		self.clients
			.claim()
			.then(() => {
				console.log("Service worker activated")
			})
			.catch(console.error)
	)
})

let state: Client

async function download(file: File): Promise<Uint8Array> {
	return await collectBytes((writer: WritableStream<Uint8Array>) =>
		state.downloadFileToWriter({
			file: file,
			writer
		})
	)
}

export async function initClient(client: StringifiedClient): Promise<void> {
	console.log("Initializing state in service worker...")

	await init(dataURItoBuffer(filenSdkRsWasmPath))

	state = fromStringified(client)

	console.log("State initialized in service worker")
}

self.addEventListener("fetch", (e: FetchEvent) => {
	try {
		const url = new URL(e.request.url)
		console.log(`Handling fetch event for ${url.pathname}`)
		switch (url.pathname) {
			case "/serviceWorker/download": {
				const file = url.searchParams.get("file")

				if (!file) {
					e.respondWith(
						new Response("No file provided", {
							status: 400
						})
					)

					return
				}

				e.respondWith(
					download(JSON.parse(decodeURIComponent(file), jsonBigIntReviver) as File).then(data => new Response(Buffer.from(data)))
				)

				break
			}
			case "/serviceWorker/init": {
				// deserialize client from query params
				const client = url.searchParams.get("stringifiedClient")

				if (client) {
					e.respondWith(
						initClient(JSON.parse(decodeURIComponent(client), jsonBigIntReviver) as StringifiedClient).then(
							() => new Response("Client initialized in SW")
						)
					)
				} else {
					e.respondWith(
						new Response("No client provided", {
							status: 400
						})
					)
				}

				break
			}
			case "/serviceWorker/ping": {
				e.respondWith(new Response("pong"))

				break
			}
		}
	} catch (err) {
		console.error(err)

		return null
	}
})

export async function collectBytes(downloadFn: (writer: WritableStream<Uint8Array>) => Promise<void>): Promise<Uint8Array> {
	const chunks: Uint8Array[] = []

	await downloadFn(
		new WritableStream<Uint8Array>({
			write(chunk: Uint8Array) {
				chunks.push(chunk)
			}
		})
	)

	const totalLength = chunks.reduce((sum, chunk) => sum + chunk.length, 0)
	const result = new Uint8Array(totalLength)
	let offset = 0

	for (const chunk of chunks) {
		result.set(chunk, offset)

		offset += chunk.length
	}

	return result
}

export function jsonBigIntReviver(_: string, value: unknown) {
	if (typeof value === "string" && value.startsWith("$bigint:") && value.endsWith("n")) {
		return BigInt(value.slice(8, -1))
	}

	return value
}

export function dataURItoBuffer(dataURI: string): ArrayBuffer {
	const parts = dataURI.split(",")

	if (parts.length !== 2) {
		throw new Error("Invalid data URI format.")
	}

	const base64Payload = parts[1]
	const binaryString = atob(base64Payload)
	const len = binaryString.length
	const bytes = new Uint8Array(len)

	for (let i = 0; i < len; i++) {
		bytes[i] = binaryString.charCodeAt(i)
	}

	return bytes.buffer
}
