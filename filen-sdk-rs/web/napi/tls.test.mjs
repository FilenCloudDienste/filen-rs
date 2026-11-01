// Online check of the Node-API addon's TLS stack (`npm run build:napi` first): one login attempt for
// an account that does not exist. Its first request goes to the API over HTTPS, so it only reaches
// the server once rustls has verified the gateway's certificate chain; the server then refuses the
// login, which is the expected outcome. A failure to send the request at all (the Reqwest kind),
// such as a certificate the addon's crypto cannot verify, fails the test.

import assert from "node:assert/strict"
import { randomUUID } from "node:crypto"
import { test } from "node:test"

import { ErrorKind, FilenSdkError, UnauthJsClient } from "../node/index.js"

test("an API request verifies the gateway's certificate and reaches the server", { timeout: 120_000 }, async () => {
	const client = UnauthJsClient.fromConfig({})
	await assert.rejects(
		client.login({
			email: `napi-tls-check-${randomUUID()}@example.com`,
			password: randomUUID(),
			twoFactorCode: undefined
		}),
		e => {
			assert.ok(FilenSdkError.hasInner(e), `not a FilenSdkError: ${e}`)
			const inner = FilenSdkError.getInner(e)
			const detail = inner.toDebugString()
			assert.notEqual(inner.kind(), ErrorKind.Reqwest, `the request was not sent: ${detail}`)
			assert.doesNotMatch(detail, /certificate|BadSignature|tls|handshake/i, detail)
			return true
		}
	)
})
