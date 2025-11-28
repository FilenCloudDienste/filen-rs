import { defineConfig } from "vite"
import { VitePWA } from "vite-plugin-pwa"
import wasm from "vite-plugin-wasm"
import { nodePolyfills } from "vite-plugin-node-polyfills"
import topLevelAwait from "vite-plugin-top-level-await"

const now = Date.now()

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
			outDir: "./",
			strategies: "injectManifest",
			workbox: {
				maximumFileSizeToCacheInBytes: Number.MAX_SAFE_INTEGER
			},
			injectRegister: false,
			manifest: false,
			injectManifest: {
				injectionPoint: undefined,
				rollupFormat: "iife",
				minify: false,
				sourcemap: false,
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
				enabled: false
			}
		}),
		topLevelAwait({
			promiseExportName: "__tla",
			promiseImportName: i => `__tla_${i}`
		})
	],
	build: {
		target: "esnext",
		sourcemap: false,
		cssMinify: "esbuild",
		minify: "esbuild",
		outDir: "./dist",
		chunkSizeWarningLimit: Infinity,
		rollupOptions: {
			output: {
				chunkFileNames() {
					return `[name].[hash].${now}.js`
				},
				entryFileNames() {
					return `[name].${now}.js`
				},
				assetFileNames() {
					return `assets/[name]-[hash].${now}[extname]`
				}
			}
		}
	},
	worker: {
		format: "es",
		plugins: () => [
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
	},
	server: {
		headers: {
			"Cross-Origin-Embedder-Policy": "require-corp",
			"Cross-Origin-Opener-Policy": "same-origin",
			"Cross-Origin-Resource-Policy": "cross-origin",
			"Access-Control-Allow-Origin": "*",
			"Access-Control-Allow-Methods": "GET, POST, PUT, DELETE, OPTIONS",
			"Access-Control-Allow-Headers": "*"
		}
	},
	preview: {
		headers: {
			"Cross-Origin-Embedder-Policy": "require-corp",
			"Cross-Origin-Opener-Policy": "same-origin",
			"Cross-Origin-Resource-Policy": "cross-origin",
			"Access-Control-Allow-Origin": "*",
			"Access-Control-Allow-Methods": "GET, POST, PUT, DELETE, OPTIONS",
			"Access-Control-Allow-Headers": "*"
		}
	},
	publicDir: "test-assets"
})
