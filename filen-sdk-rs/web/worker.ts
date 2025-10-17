import init from "./sdk-rs.js"

self.onmessage = async event => {
	// event.data[0] should be the Memory object, and event.data[1] is the value to pass into child_entry_point
	const { worker_entry_point } = await init("./sdk-rs_bg.wasm", event.data[0])
	worker_entry_point(Number(event.data[1]))
}
