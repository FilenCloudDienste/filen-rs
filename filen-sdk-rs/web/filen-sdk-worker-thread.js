import init from "./sdk-rs.js"

self.onmessage = async event => {
	const data = event.data
	const { worker_entry_point } = await init("./sdk-rs_bg.wasm", data.memory)
	worker_entry_point(data.closurePtr, data.worker)
}
