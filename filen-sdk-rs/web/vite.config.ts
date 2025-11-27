import { defineConfig } from "vite"
import { VitePWA } from "vite-plugin-pwa"
import wasm from "vite-plugin-wasm"
import { nodePolyfills } from "vite-plugin-node-polyfills"
import topLevelAwait from "vite-plugin-top-level-await"

export default defineConfig({
	plugins: [
		nodePolyfills({
			include: ["buffer", "path"],
			globals: {
				Buffer: true
			},
			protocolImports: true
		}),
		wasm(),
		VitePWA({
			srcDir: "./",
			filename: "sw.ts",
			outDir: "dist",
			strategies: "injectManifest",
			workbox: {
				maximumFileSizeToCacheInBytes: Number.MAX_SAFE_INTEGER
			},
			injectRegister: false,
			manifest: false,
			injectManifest: {
				injectionPoint: undefined,
				rollupFormat: "iife",
				minify: true,
				sourcemap: true,
				target: "es2018",
				buildPlugins: {
					vite: [
						nodePolyfills({
							include: ["buffer", "path"],
							globals: {
								Buffer: true
							},
							protocolImports: true
						}),
						wasm(),
						topLevelAwait({
							promiseExportName: "__tla",
							promiseImportName: i => `__tla_${i}`
						})
					]
				}
			},
			devOptions: {
				enabled: true
			}
		}),
		topLevelAwait({
			promiseExportName: "__tla",
			promiseImportName: i => `__tla_${i}`
		})
	],
	server: {
		headers: {
			"Cross-Origin-Embedder-Policy": "require-corp",
			"Cross-Origin-Opener-Policy": "same-origin"
		}
	},
	test: {
		// 20 minutes, the tests are contending with other tests for account locks
		// which means they can sometimes take a long time to complete
		hookTimeout: 12_000_000,
		testTimeout: 12_000_000,
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
