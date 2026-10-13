// Checks the napi addon's typings (`node/index.d.ts`, from `npm run build:napi`) against what
// uniffi-bindgen-react-native generates for the same names, so TS written against the
// react-native @filen/sdk-rs type-checks unchanged against the desktop build.
//
//   node napi/parity.mjs [--reference <dir>] [--napi <d.ts>] [--allowlist <json>] [--keep]
//
// The reference is generated from this tree: the host cdylib built with the mobile binding's
// features (`uniffi,http-provider,cache,sync-engine`), then
// `ubrn generate jsi bindings --library`.
// `--reference <dir>` takes a directory of ubrn-generated .ts instead.
//
// Every exported napi name becomes one or more type-level checks in a generated .ts file,
// `Assert<Equals<Norm<N…>, Norm<U…>>>`, one per line, which the pinned TypeScript compiles.
// An error on a check's line fails that check; a napi name ubrn does not export fails too.
// Failures listed in the allowlist (`{ "Name" | "Name.member": "reason" }`) pass, and an
// allowlisted check that passes fails the run, so the list never goes stale.

import { spawnSync } from "node:child_process"
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs"
import { createRequire } from "node:module"
import { tmpdir } from "node:os"
import { dirname, join, resolve } from "node:path"
import { fileURLToPath } from "node:url"
import { parseArgs } from "node:util"

const webDir = resolve(dirname(fileURLToPath(import.meta.url)), "..")
const sdkDir = resolve(webDir, "..")
const require = createRequire(join(webDir, "package.json"))

const { values: args } = parseArgs({
	options: {
		reference: { type: "string" },
		napi: { type: "string", default: join(webDir, "node", "index.d.ts") },
		allowlist: { type: "string", default: join(webDir, "napi", "parity-allowlist.json") },
		keep: { type: "boolean", default: false }
	}
})

function run(command, commandArgs, options = {}) {
	const result = spawnSync(command, commandArgs, { stdio: "inherit", ...options })
	if (result.error) throw result.error
	if (result.status !== 0) {
		throw new Error(`${command} ${commandArgs.join(" ")} exited with ${result.status}`)
	}
	return result
}

/** Builds the uniffi cdylib of this tree and generates ubrn's TS from it into `dir`. */
function generateReference(dir) {
	const manifest = join(sdkDir, "Cargo.toml")
	run("cargo", ["build", "--manifest-path", manifest, "--lib", "--features", "uniffi,http-provider,cache,sync-engine"])
	const metadata = run("cargo", ["metadata", "--manifest-path", manifest, "--format-version", "1", "--no-deps"], {
		stdio: ["ignore", "pipe", "inherit"],
		encoding: "utf8",
		maxBuffer: 64 * 1024 * 1024
	})
	const targetDir = JSON.parse(metadata.stdout).target_directory
	const library = {
		darwin: "libfilen_sdk_rs.dylib",
		win32: "filen_sdk_rs.dll"
	}[process.platform] ?? "libfilen_sdk_rs.so"
	const ubrn = join(dirname(require.resolve("uniffi-bindgen-react-native/package.json")), "bin", "cli.cjs")
	// ubrn reads the crate's metadata from the Cargo.toml in its working directory.
	run(
		process.execPath,
		[ubrn, "generate", "jsi", "bindings", "--library", "--no-format", "--ts-dir", dir, "--cpp-dir", join(dir, "cpp"),
			join(targetDir, "debug", library)],
		{ cwd: sdkDir }
	)
	return dir
}

/** Warns when `dir` is ubrn output shipped in another version of this package (the published
 * 0.4.38 one predates fields of this tree). */
function warnIfPublished(dir) {
	const ownVersion = JSON.parse(readFileSync(join(webDir, "package.json"), "utf8")).version
	for (let at = resolve(dir); at !== dirname(at); at = dirname(at)) {
		const manifest = join(at, "package.json")
		if (!existsSync(manifest)) continue
		const { name, version } = JSON.parse(readFileSync(manifest, "utf8"))
		if (name === "@filen/sdk-rs" && version !== ownVersion) {
			console.warn(
				`warning: the reference is the published @filen/sdk-rs ${version} output, not this tree's ` +
					`(${ownVersion}); its declarations may be stale. Leave out --reference to generate them.`
			)
		}
		return
	}
}

/**
 * The names a napi d.ts exports, each with its kind, its type parameter count and, for a class,
 * its instance members and statics. napi prints one declaration per line, members of a class
 * on their own lines indented by two spaces, so a line-based reading is enough.
 */
export function napiExports(source) {
	const items = []
	let inClass = null
	for (const line of source.split("\n")) {
		if (inClass) {
			if (line.startsWith("}")) {
				inClass = null
				continue
			}
			const member = /^  (static )?(\w+)\(/.exec(line)
			if (member) (member[1] ? inClass.statics : inClass.members).push(member[2])
			continue
		}
		const declaration =
			/^export (?:declare )?(class|enum|function|interface|type|const) (\w+)(<[^>]*>)?(?:\s*=\s*(\w+)\s*$)?/.exec(line)
		if (!declaration) continue
		const [, kind, name, typeParams, aliasOf] = declaration
		const item = { kind, name, typeParams: typeParams ? typeParams.split(",").length : 0, aliasOf }
		if (kind === "class") {
			Object.assign(item, { members: [], statics: [] })
			inClass = item
		}
		items.push(item)
	}
	return items
}

// The documented, systematic differences between napi's and ubrn's typings, normalized away on
// both sides before the identity check:
// - an optional key `k?: T` is `k: T | undefined` (napi derives `?` from an Option field);
// - `readonly` goes (ubrn freezes and marks `readonly`, napi does not);
// - symbol keys go (ubrn's `[uniffiTypeNameSymbol]` and friends);
// - enum members become their values (`${E}`), so napi's tag literal `'V'` equals `X_Tags.V`
//   and two enums with the same members and values are equal;
// - a class-like type (an object whose every member is a function) becomes a marker, so
//   napi's `JsClient` equals ubrn's `JsClientInterface` where one is used inside another type:
//   each class's members are checked on their own, and so is every interface by name;
// - a type predicate return (`obj is X`) is read as `boolean`.
// Promises, arrays, functions and built-ins (compared as they are) are recognised first, so
// they never reduce to the marker. Recursion stops at class-like types, which also keeps it
// shallow enough for TypeScript's instantiation depth.
const prelude = `
type Equals<X, Y> = (<V>() => V extends X ? 1 : 2) extends (<V>() => V extends Y ? 1 : 2) ? true : false
type Assert<T extends true> = T
type AnyFn = (...args: any[]) => any
type Keys<T> = Exclude<keyof T, symbol>
type ClassLike<T> = [Keys<T>] extends [never]
	? false
	: { [K in Keys<T>]-?: T[K] extends AnyFn ? true : false }[Keys<T>] extends true
		? true
		: false
type BuiltIn = AbortSignal | Date | ArrayBuffer | ArrayBufferView | Map<any, any> | Set<any>
type NormFn<T> = T extends (...args: infer A) => infer R ? (...args: { [K in keyof A]: Norm<A[K]> }) => Norm<R> : never
type NormObj<T> = { [K in Keys<T>]: Norm<Exclude<T[K], undefined>> | (undefined extends T[K] ? undefined : never) }
type Norm<T> = T extends string | number | bigint | boolean
	? \`\${T}\`
	: T extends null | undefined | void
		? T
		: T extends BuiltIn
			? T
			: T extends Promise<infer V>
				? Promise<Norm<V>>
				: T extends AnyFn
					? NormFn<T>
					: T extends readonly (infer E)[]
						? number extends T["length"]
							? Array<Norm<E>>
							: { -readonly [K in keyof T]: Norm<T[K]> }
						: T extends object
							? ClassLike<T> extends true
								? "<object>"
								: NormObj<T>
							: T
// A named type: an object is compared member by member, never reduced to the marker.
type Top<T> = T extends AnyFn ? Norm<T> : T extends object ? NormObj<T> : Norm<T>
`

const typeArgs = count => (count ? `<${Array(count).fill("unknown").join(", ")}>` : "")

/** One `[checkName, typeExpression]` per check for each napi export. */
export function checksFor(items) {
	const classes = new Set(items.filter(item => item.kind === "class").map(item => item.name))
	const names = new Set(items.map(item => item.name))
	const checks = []
	const check = (name, n, u) => checks.push([name, `Assert<Equals<${n}, ${u}>>`])
	for (const item of items) {
		const { name } = item
		switch (item.kind) {
			case "class":
				// ubrn must export the class too; its members are compared one by one.
				check(name, "true", `typeof U.${name} extends unknown ? true : never`)
				// ubrn types every object it returns as its `XInterface`, so napi/compat.d.ts must
				// declare that alias too; a missing one fails here, by the alias's name.
				if (!names.has(`${name}Interface`)) {
					check(`${name}Interface`, `N.${name}Interface`, `U.${name}Interface`)
				}
				for (const member of item.members) {
					check(`${name}.${member}`, `Norm<N.${name}["${member}"]>`, `Norm<U.${name}["${member}"]>`)
				}
				for (const member of item.statics) {
					check(`${name}.${member}`, `Norm<(typeof N.${name})["${member}"]>`, `Norm<(typeof U.${name})["${member}"]>`)
				}
				break
			case "enum":
			case "function":
			case "const":
				check(name, `Top<typeof N.${name}>`, `Top<typeof U.${name}>`)
				break
			default:
				if (item.aliasOf && classes.has(item.aliasOf)) {
					// `XInterface = X`: ubrn's interface lists every method of the object, napi's
					// class only the ones it exposes, so the members both have must agree.
					const shared = `Extract<keyof N.${name}, keyof U.${name}>`
					check(name, `NormObj<Pick<N.${name}, ${shared}>>`, `NormObj<Pick<U.${name}, ${shared}>>`)
				} else {
					const params = typeArgs(item.typeParams)
					check(name, `Top<N.${name}${params}>`, `Top<U.${name}${params}>`)
				}
		}
	}
	return checks
}

/** Runs the parity check; returns the failures that are not allowlisted plus stale entries. */
function main() {
	const work = mkdtempSync(join(tmpdir(), "filen-napi-parity-"))
	try {
		let reference = args.reference
		if (reference) warnIfPublished(reference)
		else reference = generateReference(join(work, "ubrn"))
		reference = resolve(reference)

		const napi = resolve(args.napi)
		const items = napiExports(readFileSync(napi, "utf8"))
		if (!items.length) throw new Error(`no exports found in ${napi}`)
		const allowlist = existsSync(args.allowlist) ? JSON.parse(readFileSync(args.allowlist, "utf8")) : {}

		// The react-native package re-exports every generated module but the FFI ones.
		const modules = readdirSync(reference).filter(file => file.endsWith(".ts") && !file.endsWith("-ffi.ts"))
		if (!modules.length) throw new Error(`no ubrn .ts in ${reference}`)
		writeFileSync(
			join(work, "ubrn.ts"),
			modules.map(file => `export * from ${JSON.stringify(join(reference, file.replace(/\.ts$/, "")))}`).join("\n") + "\n"
		)

		const checks = checksFor(items)
		const header = [
			`import type * as N from ${JSON.stringify(napi.replace(/\.d\.ts$/, ""))}`,
			`import type * as U from "./ubrn"`,
			...prelude.split("\n")
		]
		const firstLine = header.length + 1
		writeFileSync(
			join(work, "parity.check.ts"),
			[...header, ...checks.map(([, expression], i) => `export type Check${i} = ${expression}`)].join("\n") + "\n"
		)
		writeFileSync(
			join(work, "tsconfig.json"),
			JSON.stringify({
				compilerOptions: {
					strict: true,
					noEmit: true,
					target: "es2022",
					module: "esnext",
					moduleResolution: "bundler",
					lib: ["es2022", "dom"],
					types: [],
					skipLibCheck: true,
					paths: {
						"uniffi-bindgen-react-native": [
							join(dirname(require.resolve("uniffi-bindgen-react-native/package.json")), "typescript", "src", "index.ts")
						]
					}
				},
				files: ["parity.check.ts"]
			})
		)

		const tsc = join(dirname(require.resolve("typescript/package.json")), "bin", "tsc")
		const version = spawnSync(process.execPath, [tsc, "--version"], { encoding: "utf8" }).stdout.trim()
		console.log(`checking ${checks.length} declarations of ${items.length} napi exports with TypeScript ${version.replace(/^Version /, "")}`)
		const result = spawnSync(process.execPath, [tsc, "--project", join(work, "tsconfig.json"), "--pretty", "false"], {
			encoding: "utf8",
			maxBuffer: 64 * 1024 * 1024
		})
		const failed = new Map()
		const elsewhere = []
		for (const line of result.stdout.split("\n").filter(Boolean)) {
			const error = /^(.*)\((\d+),\d+\): (error .*)$/.exec(line)
			const index = error && error[1].endsWith("parity.check.ts") ? Number(error[2]) - firstLine : -1
			if (index >= 0 && index < checks.length) {
				const name = checks[index][0]
				failed.set(name, [...(failed.get(name) ?? []), error[3]])
			} else if (error && !error[1].endsWith("parity.check.ts")) {
				// Errors outside the checks say nothing about parity: in ubrn's runtime sources
				// (type-checked through the path mapping), or in the ubrn.ts barrel, where a name
				// two generated modules export (DateTime) is ambiguous and left out. Each is
				// printed, so a new one is seen.
				elsewhere.push(line)
			} else {
				failed.set(`<${line}>`, [])
			}
		}
		if (result.status !== 0 && !failed.size && !elsewhere.length) {
			throw new Error(`tsc exited with ${result.status}: ${result.stderr || result.stdout}`)
		}
		for (const line of elsewhere) console.log(`ignored  ${line}`)

		const problems = []
		for (const [name, errors] of failed) {
			if (name in allowlist) console.log(`allowed  ${name}: ${allowlist[name]}`)
			else problems.push(`FAIL     ${name}\n${errors.map(error => `           ${error}`).join("\n")}`)
		}
		const checkNames = new Set(checks.map(([name]) => name))
		for (const name of Object.keys(allowlist)) {
			if (!failed.has(name)) {
				problems.push(
					checkNames.has(name)
						? `STALE    ${name}: passes now, remove it from the allowlist`
						: `STALE    ${name}: no such check, remove it from the allowlist`
				)
			}
		}
		for (const problem of problems) console.log(problem)
		console.log(problems.length ? `${problems.length} parity problems` : "napi typings match ubrn's")
		if (args.keep) console.log(`kept ${work}`)
		return problems.length ? 1 : 0
	} finally {
		if (!args.keep) rmSync(work, { recursive: true, force: true })
	}
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
	process.exitCode = main()
}
