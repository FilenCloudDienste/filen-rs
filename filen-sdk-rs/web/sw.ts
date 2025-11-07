/// <reference types="@types/serviceworker" />

import init, { Client, fromStringified, type StringifiedClient, type File } from "./service-worker/sdk-rs.js"
import filenSdkRsWasmPath from "./service-worker/sdk-rs_bg.wasm?url"

self.addEventListener("install", () => {
	self.skipWaiting().catch(console.error)
})

self.addEventListener("activate", (event: ExtendableEvent) => {
	event.waitUntil(self.clients.claim())
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
	await init({ module_or_path: filenSdkRsWasmPath })
	state = fromStringified(client)
}

self.addEventListener("fetch", (e: FetchEvent) => {
	try {
		const url = new URL(e.request.url)
		switch (url.pathname) {
			case "/serviceWorker/download/": {
				const file = url.searchParams.get("file")
				if (!file) {
					e.respondWith(new Response("No file provided", { status: 400 }))
					return
				}
				e.respondWith(download(JSON.parse(file, jsonBigIntReviver) as File).then(data => new Response(Buffer.from(data))))
				break
			}
			case "/serviceWorker/init/": {
				// deserialize client from query params
				const client = url.searchParams.get("stringifiedClient")
				if (client) {
					e.respondWith(
						initClient(JSON.parse(client, jsonBigIntReviver) as StringifiedClient).then(
							() => new Response("Client initialized in SW")
						)
					)
				} else {
					e.respondWith(new Response("No client provided", { status: 400 }))
				}
			}
		}
	} catch (err) {
		console.error(err)

		return null
	}
})

async function collectBytes(downloadFn: (writer: WritableStream<Uint8Array>) => Promise<void>): Promise<Uint8Array> {
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

export function jsonBigIntReviver(_: string, value: unknown) {
	if (typeof value === "string" && value.startsWith("$bigint:") && value.endsWith("n")) {
		return BigInt(value.slice(8, -1))
	}

	return value
}
