import { defineConfig } from "vite"
import wasm from "vite-plugin-wasm"

export default defineConfig({
	plugins: [wasm()],
	test: {
		hookTimeout: 60000,
		testTimeout: 30000
	}
})
