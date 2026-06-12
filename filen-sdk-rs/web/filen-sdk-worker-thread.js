import init from "./sdk-rs.js"

self.onmessage = async event => {
	const data = event.data
	const { worker_entry_point } = await init({ module_or_path: "./sdk-rs_bg.wasm", memory: data.memory })
	worker_entry_point(data.closurePtr, data.worker)
}
