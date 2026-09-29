---
name: security
description: >
    Use when writing or reviewing code that handles: file uploads or filesystem paths,
    credentials/API keys/secrets, cryptography, user input, authentication or
    authorisation, outbound requests to user-supplied URLs, or dependency changes.
    Checks for: injection, path traversal, hardcoded secrets, insecure crypto, missing
    ownership checks, SSRF, mass assignment, and information leakage. Also flags
    security issues in adjacent code.
---

# Security — Write Secure Code by Default

Security bugs are bugs. A security flaw in production is worse than a logic bug. Every change
either creates attack surface or reduces it — know which before you write it.

## Step 0 — Find what the project already has

Read the manifest for the language you are in (the `Read` tool, one known file):

| Manifest | Toolchain | Look for existing security deps |
|---|---|---|
| `Cargo.toml` | cargo | `argon2`, `ring`, `rustls`, `hmac`, `sha2`, `aes-gcm`, `rand` |
| `package.json` | npm / yarn / bun | `helmet`, `cors`, `csrf`, `zod`, `validator`, `bcrypt` |
| `pyproject.toml` / `requirements.txt` | ruff / pytest | `cryptography`, `passlib`, `pydantic`, `bleach` |
| `go.mod` | go | `crypto/*`, `jwt`, `bcrypt` |
| `Package.swift` | swift | `CryptoKit`, Keychain wrappers |
| `Gemfile` | bundler | `devise`, `bcrypt`, `rack-attack` |

Then find the existing patterns — always `git grep` with an extension pathspec:

```bash
git grep -n -iE "auth|validate|sanitize|escape|middleware|guard|keychain" -- '*.rs' '*.ts' '*.py' '*.go' '*.swift' ':!*.d.ts'
```

- Definitions and call sites of a security helper: the `LSP` tool (`workspaceSymbol`,
  `goToDefinition`, `findReferences`) before any text search.
- `Grep` / `Glob` tools if available; they are not in every permission mode, so the
  `git grep` form above is the one to rely on.
- Untracked files only: `grep -rn "PATTERN" --include="*.rs" /abs/path/to/subtree` — an
  absolute subtree path, never the repo root, never `.`.
- Never `cd`. Never read environment files; if you need a value from one, stop and ask.

**Use what the project already has.** Existing validator, existing auth middleware, existing
crypto crate. Never introduce a parallel security mechanism alongside one that exists.

Map the trust boundary: **trusted** = values you generated and validated yourself; **untrusted**
= everything else — HTTP bodies, query params, headers, uploaded files, CLI args, FFI/IPC
payloads, deep links and intents, third-party API responses, and database values a user wrote.

## The Golden Rule

**Never trust data you did not create yourself.** Validate untrusted input against a strict
allowlist of what is acceptable and reject what does not match. Never sanitise your way out of
a validation failure.

---

## 1. Input Validation and Injection

Validate at every entry point, before any use: **type**, **format** (UUID, email, filename,
URL), **range/length**, **allowlist** membership. Reject; never coerce untrusted input.

### SQL injection — parameterised queries, always

```
# ❌ String interpolation in SQL, in any language
"SELECT * FROM users WHERE email = '" + email + "'"
f"SELECT * FROM users WHERE email = '{email}'"

# ✅ Parameterised — the driver escapes; input is never parsed as SQL
db.query("SELECT * FROM users WHERE email = ?", [email])    # positional
db.query("SELECT * FROM users WHERE email = $1", [email])   # numbered
db.execute("... WHERE email = :email", {email})             # named
```

Every statement, no exceptions. With an ORM use its query builder; never drop to raw string SQL
carrying user data.

### Path traversal — confine to a base directory

```
# ❌ User controls where on disk you read/write
read_file("/uploads/" + user_supplied_name)

# ✅ Resolve, then verify confinement
base = canonicalize("/uploads")
target = canonicalize(join(base, user_supplied_name))
if not target.starts_with(base + separator): reject("Invalid path")
read_file(target)
```

Canonicalize *then* compare — `..`, symlinks, and encoded separators all collapse at that step.

### Shell injection — never build a shell string

```
# ❌ The shell parses the whole string
system("convert " + filename + " out.png")

# ✅ Argument array, no shell
run(["convert", filename, "out.png"])
```

Better still: use a library instead of shelling out.

### Other injection classes

- **Template injection**: never render user-supplied strings as templates.
- **XML/XXE**: disable external entity resolution; prefer JSON.
- **LDAP**: parameterised queries or RFC 4515 escaping.
- **Deserialization**: never feed untrusted bytes to a general-purpose deserialiser
  (`pickle`, Java `ObjectInputStream`, Ruby `Marshal`). Use JSON/Protobuf/MessagePack with an
  explicit schema, and cap nesting depth and length.

---

## 2. Authentication and Authorisation

- **Verify identity on every request** — never assume a session is still valid.
- **Constant-time comparison** for tokens, MACs, and secrets — `==` leaks length and prefix
  timing (`subtle`/`crypto.timingSafeEqual`/`hmac.compare_digest`).
- **Pin token algorithms** — with JWTs, list the accepted algorithms explicitly; leaving it open
  allows `alg: none` and RS→HS confusion.
- **Expire and revoke** — finite lifetimes, plus revocation for sensitive operations.
- **Hash passwords with argon2id** (bcrypt/scrypt acceptable). SHA-\*, MD5, and plaintext are
  never acceptable, at any iteration count.

Authorisation is separate from authentication and is where most real bugs live:

```
# ❌ Authenticated, but any user can delete any file
delete_file(file_id)

# ✅ Ownership enforced in the data-access layer
file = db.find(file_id, owner_id = current_user.id)   # None if not owned
if not file: reject(404)
delete_file(file.id)
```

- **Server-side always** — the client can send any request regardless of what its UI shows.
- **Least privilege** — a key or session gets exactly the scope its function needs.
- **Default deny** — a missing or failed check must end in denial.

**Sessions**: generate IDs from the OS CSPRNG; regenerate on privilege change (login, role
change); invalidate server-side on logout, not just the cookie; set `HttpOnly`, `Secure`,
`SameSite=Strict|Lax`.

---

## 3. Secrets and Credentials

```
# ❌ Hardcoded — it will reach version control
API_KEY = "sk-prod-abc123"

# ✅ From the environment or a secret manager; fail loudly when missing
api_key = require_env("API_KEY")   # error out, never a silent default
```

Applies to API keys, DB passwords, JWT/signing secrets, encryption keys, private keys, OAuth
client secrets, and internal service tokens.

- **Never log secrets.** Allowlist the fields you log; never log a whole request body, config
  object, or error that may embed credentials or a URL with a token in it.
- **Never put secrets in URLs.** Every proxy, CDN, browser history, and access log records them.
  Use headers or the body.
- **Check before committing** (no file arguments, so it is safe to run anywhere):

```bash
git diff --staged | grep -inE "password|secret|api.?key|token|private.?key|BEGIN [A-Z ]*PRIVATE"
```

Environment files must never be read, committed, or printed. If you need a value from one, ask.

---

## 4. Cryptography

Use standard, audited implementations. Custom crypto is broken crypto. Check the manifest for
what the project already depends on before adding anything.

| Purpose | Recommended | Never use |
|---|---|---|
| Password hashing | argon2id, bcrypt, scrypt | MD5, SHA-\*, fast hashes, plaintext |
| Symmetric encryption | AES-256-GCM, ChaCha20-Poly1305 | ECB (any cipher), RC4, DES, 3DES |
| Asymmetric encryption | RSA-OAEP (≥2048), X25519 | RSA-PKCS1v1.5, raw RSA |
| Digital signatures | Ed25519, ECDSA P-256, RSA-PSS | RSA-PKCS1v1.5 |
| Integrity / MAC | HMAC-SHA256, HMAC-SHA512 | MD5, SHA-1, CRC32 |
| Random tokens | OS CSPRNG | `Math.random()`, `rand()`, time-seeded PRNGs |
| Key derivation | HKDF, Argon2, PBKDF2 (≥600k) | plain hash of password + salt |
| TLS | 1.3 preferred, 1.2 floor | SSL, TLS 1.0/1.1 |

Secure randomness — the OS CSPRNG, in every language:

```
Rust:   rand::rngs::OsRng.fill_bytes(&mut buf)   /  getrandom::fill(&mut buf)
Node:   crypto.randomBytes(32)
Python: secrets.token_hex(32)  /  os.urandom(32)
Go:     crypto/rand.Read(buf)
Swift:  SecRandomCopyBytes  /  SystemRandomNumberGenerator
```

**Encryption correctness**: use authenticated encryption (unauthenticated CBC/CTR lets an
attacker tamper undetected); never reuse an IV/nonce under one key; store the IV with the
ciphertext (unique, not secret); verify the tag *before* trusting plaintext.

---

## 5. HTTP and Network

- **Headers on every response**: `Content-Security-Policy`, `X-Frame-Options: DENY`,
  `X-Content-Type-Options: nosniff`, `Strict-Transport-Security`.
- **CORS**: explicit origin allowlist. Never `*` on anything credentialed.
- **Rate limiting**: on auth, password reset, and any expensive or enumerable endpoint.
- **HTTPS only**, HSTS on, old TLS versions disabled.

### SSRF — validate URLs before fetching them for a user

```
# ❌ Blind fetch of a user-supplied URL
response = http.get(user_url)

# ✅ Parse, allowlist, then fetch
parsed = parse_url(user_url)
if parsed.host not in ALLOWED_HOSTS: reject("Host not allowed")
if parsed.scheme != "https":         reject("HTTPS only")
response = http.get(parsed)
```

Where an allowlist is impossible, block private and link-local ranges (`10.x`, `172.16-31.x`,
`192.168.x`, `127.x`, `::1`, `169.254.169.254`) — and re-check after redirects.

### Mass assignment

```
# ❌ User can set role, owner_id, balance...
db.update(id, request.body)

# ✅ Explicit field extraction
db.update(id, { display_name: body.display_name, avatar_url: body.avatar_url })
```

---

## 6. File Handling

- **Detect type from content** (magic bytes), never from the extension or `Content-Type` — both
  are attacker-controlled.
- **Enforce a size limit** before reading into memory; stream large files.
- **Generate the storage name yourself** (UUID or content hash); keep the original name as data
  only, for display.
- **Store outside the served root**; serve only deliberately, after validation.
- **Parse complex formats in isolation** — image/video/document decoders are memory-unsafe
  attack surface; sandbox or resource-limit them.

---

## 7. Dependencies

Every dependency is attack surface. Before adding one: is it maintained, does it have a
disclosure process, is there a lighter or already-present alternative? After any dependency
change, run the audit for the manifest you touched:

```bash
cargo audit                        # Cargo.toml
npm audit                          # package.json  (yarn/bun/pnpm equivalents)
pip-audit                          # pyproject.toml / requirements.txt
govulncheck ./...                  # go.mod
bundle audit                       # Gemfile
dotnet list package --vulnerable   # .NET
```

Commit the lockfile. A vulnerability in a dependency is a vulnerability in your code.

---

## 8. Errors and Information Leakage

Never return to a caller: stack traces, internal paths or module names, query structure or
column names, framework versions, internal hostnames or IPs.

```
# ❌ return { error: e.message, trace: e.stacktrace, query: failed_sql }
# ✅ log_internally(e, request_context); return { error: "An unexpected error occurred" }
```

Verbose errors may be gated on an explicit environment flag — never on anything the caller
controls.

---

## 9. Platform Notes

- **Mobile (iOS/Android)**: Keychain / Keystore for tokens and credentials — never plain
  preferences, plain files, or unencrypted local DBs. Validate everything arriving via deep
  links, intents, and share extensions.
- **Browser**: `HttpOnly` cookies over `localStorage` for session tokens; anything JS can read,
  XSS can steal. Electron renderers are web contexts with the same risks.
- **Server**: prefer a secret manager over long-lived environment variables in production.
- **CLI**: never write secrets to stdout, log files, or shell history; prompt securely.
- **Native/Rust**: keep `unsafe` out of parsers fed by untrusted bytes; treat every FFI boundary
  as a trust boundary and validate lengths and pointers on the way in; check arithmetic on
  attacker-controlled sizes and offsets.

---

## 10. Before Calling It Done

- Untrusted input validated by type, format, range, allowlist — and rejected, not coerced.
- Queries parameterised; paths canonicalized and confined; subprocesses use argument arrays;
  user-supplied URLs allowlisted before fetch.
- Every read and write verifies ownership server-side; token comparisons constant-time;
  passwords argon2id/bcrypt/scrypt.
- No hardcoded secrets; none in logs, URLs, or error responses; startup fails loudly on missing.
- Tokens from the OS CSPRNG; encryption authenticated; no custom crypto.
- Security headers, CORS allowlist, and rate limits on the endpoints that need them.
- Dependency audit clean after any manifest change.

Then run the project's own gate — `cargo fmt --check` + `cargo clippy` + `cargo test --lib` for
a Cargo workspace, the equivalent lint/typecheck/test scripts for the other manifests.

---

## When You Spot a Vulnerability

Flag it immediately, even when it is outside the task you were given:

> Note: while working on X I noticed Y is vulnerable to Z (e.g. unsanitised input reaching a
> shell command at `path/file.rs:NN`). I have not changed it — out of scope here — but it should
> be fixed before this ships.

Never silently work around a vulnerability, and never leave one unflagged. Security debt compounds.

In filen-rs, the maintainer has rejected `zeroize` and key wiping (secret-holding types get a redacting `Debug` instead). New `unsafe`, vendored crates and `[patch]` need the maintainer's approval; see the Contributing section of CLAUDE.md.
