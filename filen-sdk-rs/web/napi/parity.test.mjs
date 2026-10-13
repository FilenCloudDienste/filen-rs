// Self-test of napi/parity.mjs on two small fixture pairs, so that a compiler upgrade that makes
// its `Equals`/`Norm` pass everything (or nothing) fails here instead of going unnoticed.

import assert from "node:assert/strict"
import { spawnSync } from "node:child_process"
import { mkdtempSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { dirname, join } from "node:path"
import { after, before, test } from "node:test"
import { fileURLToPath } from "node:url"

const napiDir = dirname(fileURLToPath(import.meta.url))
let allowlist

before(() => {
	allowlist = join(mkdtempSync(join(tmpdir(), "filen-napi-parity-test-")), "allowlist.json")
	writeFileSync(allowlist, "{}")
})

after(() => rmSync(dirname(allowlist), { recursive: true, force: true }))

function parity(fixture) {
	const dir = join(napiDir, "parity-fixtures", fixture)
	const run = spawnSync(
		process.execPath,
		[join(napiDir, "parity.mjs"), "--napi", join(dir, "napi.d.ts"), "--reference", join(dir, "ubrn"), "--allowlist", allowlist],
		{ encoding: "utf8" }
	)
	const failed = [...run.stdout.matchAll(/^FAIL +(\S+)$/gm)].map(match => match[1]).sort()
	return { status: run.status, failed, output: run.stdout + run.stderr }
}

test("typings that differ only in what is normalized pass", () => {
	const { status, failed, output } = parity("matching")
	assert.deepEqual(failed, [], output)
	assert.equal(status, 0, output)
	assert.match(output, /checking 11 declarations of 8 napi exports/)
})

test("a number for a bigint, a missing method and a missing class alias fail, each by its name", () => {
	const { status, failed, output } = parity("mismatching")
	assert.deepEqual(failed, ["Config", "Listener", "ThingInterface"], output)
	assert.equal(status, 1, output)
})
