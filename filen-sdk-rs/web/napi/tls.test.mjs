// Online check of the Node-API addon's TLS stack (`npm run build:napi` first): one health check of
// the gateway, an unauthenticated request over HTTPS that only reaches the server once rustls has
// verified the gateway's certificate chain, and is sent once, without retries. A failure to send it
// (the Reqwest kind) or a certificate the addon cannot verify (the TlsVerification kind) fails it.

import assert from "node:assert/strict"
import { test } from "node:test"

import { FilenSdkError, UnauthJsClient } from "../node/index.js"

test("the gateway's health check verifies its certificate and reaches the server", { timeout: 120_000 }, async () => {
	try {
		await UnauthJsClient.fromConfig({}).health()
	} catch (e) {
		const detail = FilenSdkError.hasInner(e) ? FilenSdkError.getInner(e).toDebugString() : String(e)
		assert.fail(`the health check failed: ${detail}`)
	}
})
