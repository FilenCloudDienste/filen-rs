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

// Module state lives only as long as this worker instance: the browser stops an idle service
// worker (Firefox after 30 s) and starts a fresh one on the next fetch, so a client set by
// /serviceWorker/init is gone by the time a page that waited on the network comes back.
let state: Client | undefined

async function download(client: Client, file: File): Promise<Uint8Array> {
	return await collectBytes((writer: WritableStream<Uint8Array>) =>
		client.downloadFileToWriter({
			file: file,
			writer
		})
	)
}

export async function initClient(client: StringifiedClient): Promise<Client> {
	console.log("Initializing state in service worker...")

	await init(dataURItoBuffer(filenSdkRsWasmPath))

	state = fromStringified(client)

	console.log("State initialized in service worker")

	return state
}

// The client this instance holds, or the one the request carries as `stringifiedClient` when
// the worker was restarted since /serviceWorker/init.
async function ensureClient(url: URL): Promise<Client> {
	if (state) {
		return state
	}

	const client = url.searchParams.get("stringifiedClient")

	if (!client) {
		throw new Error("service worker was restarted and the request carries no stringifiedClient")
	}

	return await initClient(JSON.parse(decodeURIComponent(client), jsonBigIntReviver) as StringifiedClient)
}

// A rejected respondWith promise reaches the page as an opaque "NetworkError when attempting to
// fetch resource" / "Failed to fetch"; answer with a 500 that names the failure instead.
// Message and stack both: Firefox's `stack` does not repeat the message, Chromium's does.
function respond(e: FetchEvent, handler: () => Promise<Response>) {
	e.respondWith(
		handler().catch(
			(err: unknown) =>
				new Response(`service worker failed: ${err instanceof Error ? `${err.message}\n${err.stack ?? ""}` : String(err)}`, {
					status: 500
				})
		)
	)
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

					break
				}

				respond(e, async () => {
					const client = await ensureClient(url)
					const data = await download(client, JSON.parse(decodeURIComponent(file), jsonBigIntReviver) as File)

					return new Response(Buffer.from(data))
				})

				break
			}
			case "/serviceWorker/init": {
				// deserialize client from query params
				const client = url.searchParams.get("stringifiedClient")

				if (client) {
					respond(e, async () => {
						await initClient(JSON.parse(decodeURIComponent(client), jsonBigIntReviver) as StringifiedClient)

						return new Response("Client initialized in SW")
					})
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
