import { defineConfig } from "vite"
import { VitePWA } from "vite-plugin-pwa"
import wasm from "vite-plugin-wasm"
import { nodePolyfills } from "vite-plugin-node-polyfills"

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
			outDir: "./test-assets/",
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
				target: "esnext",
				buildPlugins: {
					vite: [
						nodePolyfills({
							include: ["buffer", "path"],
							globals: {
								Buffer: true
							},
							protocolImports: true
						}),
						wasm()
					]
				}
			},
			devOptions: {
				enabled: false
			}
		})
	],
	// Under Vite 8, vite-plugin-node-polyfills injects these shim imports into pre-bundled deps
	// but excludes them from the optimizer, so a cold dev server discovers them mid-run and
	// reloads the page, which loses vitest's runner ("Vitest failed to find the runner").
	optimizeDeps: {
		include: [
			"vite-plugin-node-polyfills/shims/buffer",
			"vite-plugin-node-polyfills/shims/global",
			"vite-plugin-node-polyfills/shims/process"
		]
	},
	build: {
		target: "esnext",
		sourcemap: false,
		cssMinify: "lightningcss",
		minify: "oxc",
		outDir: "./dist",
		chunkSizeWarningLimit: Infinity,
		rolldownOptions: {
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
			wasm()
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
