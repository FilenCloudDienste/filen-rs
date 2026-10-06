import { defineConfig } from "vite"

// The test service worker, built the way filen-web builds its own (packages/filen-web/vite.sw.config.ts
// in filen-ts): sw.ts and the SDK's service-worker glue as one ES module, with the wasm the glue
// locates through import.meta.url emitted beside it. Both land in the public dir, which the vitest
// server serves at the root, so the test registers /sw.js.
export default defineConfig({
	publicDir: false,
	build: {
		outDir: "test-assets",
		// test-assets also holds the committed fixture images
		emptyOutDir: false,
		minify: "oxc",
		rolldownOptions: {
			input: "sw.ts",
			output: {
				format: "es",
				entryFileNames: "sw.js",
				// Unhashed, so a rebuild replaces it, and prefixed, because /sdk-rs_bg.wasm is the page's
				// threaded binary, which the worker's single-threaded glue cannot instantiate.
				assetFileNames: "sw-[name][extname]"
			}
		}
	}
})
