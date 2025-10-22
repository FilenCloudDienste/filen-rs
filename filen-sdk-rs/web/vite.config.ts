import { defineConfig } from "vite"
import wasm from "vite-plugin-wasm"

export default defineConfig({
	plugins: [wasm()],
	server: {
		headers: {
			"Cross-Origin-Embedder-Policy": "require-corp",
			"Cross-Origin-Opener-Policy": "same-origin"
		}
	},
	test: {
		hookTimeout: 60000,
		testTimeout: 30000,
		browser: {
			enabled: true,
			headless: true,
			provider: "playwright",

			instances: [
				{ browser: "chromium" },
				{ browser: "firefox" }
				// running the tests in parallel causes issues becaues they share the account
				// so we do it manually in the CI
				// but we have to specify a browser here or vitest complains
			]
		}
	},
	publicDir: "test-assets"
})
