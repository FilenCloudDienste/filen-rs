import init from "./sdk-rs.js"

self.onmessage = async event => {
	const data = event.data
	// Rebase this worker's clock onto the spawning page's time domain: `performance.now()` is
	// relative to each context's OWN timeOrigin, but the SDK shares timer and rate-limiter state
	// (wasmtimer's global wheel, governor's limiter) across threads through shared wasm memory.
	// Mixed clock domains skew those readings by the page-to-worker startup gap, which stalls
	// timers and rate limits for exactly that long. Workers spawned by wasm-bindgen-rayon do not
	// run this script and stay unpatched — rayon jobs are pure CPU work and create no timers.
	if (typeof data.mainTimeOrigin === "number") {
		const delta = performance.timeOrigin - data.mainTimeOrigin
		if (delta !== 0) {
			const originalNow = performance.now.bind(performance)
			performance.now = () => originalNow() + delta
		}
		globalThis.__filenMainTimeOrigin = data.mainTimeOrigin
	}
	const { worker_entry_point } = await init({ module_or_path: "./sdk-rs_bg.wasm", memory: data.memory })
	worker_entry_point(data.closurePtr, data.worker)
}
