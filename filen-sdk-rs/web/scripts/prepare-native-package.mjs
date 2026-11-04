// Readies the react-native side of the package; runs after the ubrn iOS builds and before every
// pack/publish. Idempotent.
//
// - Copies the workspace's Cargo.lock, which the native libraries were built with, next to
//   package.json: the app checks the rustls-platform-verifier-android version it pins against
//   the AAR it bundles.
// - Declares the Security framework in ubrn's generated SdkRs.podspec: the xcframework is a
//   static library, and its platform certificate verifier (SecTrust) needs Security linked.

import { copyFileSync, existsSync, readFileSync, writeFileSync } from "node:fs"
import { dirname, join, resolve } from "node:path"
import { fileURLToPath } from "node:url"

const webDir = resolve(dirname(fileURLToPath(import.meta.url)), "..")

copyFileSync(join(webDir, "..", "..", "Cargo.lock"), join(webDir, "Cargo.lock"))

const podspec = join(webDir, "SdkRs.podspec")
if (existsSync(podspec)) {
	const source = readFileSync(podspec, "utf8")
	if (!/^\s*s\.frameworks\s*=\s*"Security"\s*$/m.test(source)) {
		const vendored = /^(\s*)s\.vendored_frameworks\s*=.*$/m
		const match = source.match(vendored)
		if (!match) {
			throw new Error(`${podspec}: no s.vendored_frameworks line to declare the Security framework after`)
		}
		writeFileSync(podspec, source.replace(vendored, `$&\n${match[1]}s.frameworks = "Security"`))
	}
}
