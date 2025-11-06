/// <reference types="@types/serviceworker" />

import init, {
	Client,
    initThreadPool,
	login,
} from "./sdk-rs.js"
import filenSdkRsWasmPath from "./sdk-rs_bg.wasm?url"

self.addEventListener("install", () => {
	self.skipWaiting().catch(console.error)
})

self.addEventListener("activate", (event: ExtendableEvent) => {
	event.waitUntil(self.clients.claim())
})

let state: Client

export async function waitForFilenSdkRsWasmInit(): Promise<Client> {
    await init(fetch(filenSdkRsWasmPath))
    await initThreadPool(2) // idk maybe change

    if (!state) {
        state = await login({
            email: import.meta.env.VITE_TEST_EMAIL,
            password: import.meta.env.VITE_TEST_PASSWORD
        })
    }

    return state
}

export async function stream(e: FetchEvent): Promise<Response> {
	const state = await waitForFilenSdkRsWasmInit()

    console.log(state)

	// do some stuff here

    return new Response("stream response")
}

self.addEventListener("fetch", (e: FetchEvent) => {
	try {
		const url = new URL(e.request.url)

		switch (url.pathname) {
			case "/serviceWorker/download": {
				e.respondWith(stream(e))

				break
			}
		}
	} catch (err) {
		console.error(err)

		return null
	}
})
