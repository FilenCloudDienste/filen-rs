import { playwright } from "@vitest/browser-playwright"
import { defineConfig, mergeConfig } from "vitest/config"
import viteConfig from "./vite.config"

export default defineConfig({
	...mergeConfig(viteConfig, {
		test: {
			hookTimeout: 3600_000,
			testTimeout: 3600_000,
			teardownTimeout: 3600_000,
			browser: {
				enabled: true,
				headless: true,
				provider: playwright({
					actionTimeout: 3600_000
				}),
				instances: [
					{
						browser: "chromium"
					},
					{
						browser: "firefox"
					}
				]
			},
		},
	}),
})
