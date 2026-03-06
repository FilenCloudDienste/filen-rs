---
name: security
description: CRITICAL: Always use this skill, no matter what task you are working on!
---

# Cybersecurity — Write Secure Code by Default

Security vulnerabilities are bugs. Treat them with the same priority as correctness bugs —
a security flaw in production is always worse than a logic bug. Every piece of code either
creates attack surface or reduces it. Think about which one you are doing before you write it.

---

## Step 0 — Understand the security posture before writing

Before writing any code that touches external data, users, files, or the network:

```bash
# Find existing auth, validation, and security patterns in the project
grep -r "auth\|validate\|sanitize\|escape\|middleware\|guard" src/ | head -20

# Find what security libraries are already in use
cat package.json 2>/dev/null | grep -E "helmet|cors|csrf|validator|sanitize"
cat Cargo.toml 2>/dev/null | grep -E "argon|hmac|sha|ring|rustls"
cat requirements.txt pyproject.toml 2>/dev/null | grep -E "cryptography|passlib|bleach"
cat go.mod 2>/dev/null | grep -E "crypto|jwt|bcrypt"
cat Gemfile 2>/dev/null | grep -E "devise|bcrypt|rack-attack"

# Understand how existing endpoints or handlers deal with input
find . -name "*.ts" -o -name "*.py" -o -name "*.go" -o -name "*.rs" \
  | xargs grep -l "request\|req\|input\|param" 2>/dev/null | head -5
```

**Use what the project already has.** If it has a validation library, use it. If it has an
auth middleware, apply it. Never introduce a parallel security mechanism alongside an existing one.

Map the trust boundaries before writing:

- **Trusted**: your own validated, sanitised output; values you generated internally
- **Untrusted**: everything else — HTTP bodies, query params, headers, uploaded files, CLI args,
  environment input, database values that users wrote, third-party API responses, IPC messages

---

## The Golden Rule

**Never trust data you did not create yourself.**

Treat all external input as hostile until it has been explicitly validated against a strict
allowlist of what is acceptable. Reject anything that doesn't match. Never try to sanitise
your way out of a validation failure — reject it outright.

---

## 1. Input Validation and Injection

### Validate at the boundary — before any use

Every entry point that accepts external data must validate it before passing it anywhere:

- **Type** — is it the right type? (string, integer, boolean — not just "any value")
- **Format** — does it match the expected pattern? (UUID, email, filename, URL)
- **Range / length** — is it within acceptable bounds?
- **Allowlist** — if it must be one of a known set of values, check against that set

Reject inputs that don't pass. Never attempt to fix or coerce invalid input from untrusted sources.

### SQL injection — parameterised queries, always

Never interpolate external data into query strings. Use parameterised queries or prepared
statements in every language and every database driver:

```
# ❌ Any language — string interpolation in SQL
"SELECT * FROM users WHERE email = '" + email + "'"
f"SELECT * FROM users WHERE email = '{email}'"
`SELECT * FROM users WHERE email = '${email}'`

# ✅ Parameterised — the driver handles escaping, input is never interpreted as SQL
db.query("SELECT * FROM users WHERE email = ?", [email])        # MySQL style
db.query("SELECT * FROM users WHERE email = $1", [email])       # PostgreSQL style
db.execute("SELECT * FROM users WHERE email = :email", {email}) # named params
```

This applies to every query — SELECT, INSERT, UPDATE, DELETE. No exceptions.
If using an ORM, use its query builder. Never drop to raw string SQL with user data.

### Path traversal — confine file access to a known base directory

Never use unsanitised user input to construct file paths. An attacker who controls a path
can read or write any file the process has access to, including config files and credentials.

The fix is always the same regardless of language:

1. Resolve the user-supplied path relative to a safe base directory
2. Verify the resolved absolute path still starts with the base directory
3. Reject anything that doesn't

```
# ❌ User controls where on the filesystem you read/write
read_file("/uploads/" + user_supplied_filename)

# ✅ Resolve and verify confinement — pseudocode applicable to any language
base = resolve("/uploads")
requested = resolve(base + "/" + user_supplied_filename)
if not requested.starts_with(base + separator):
    reject("Invalid path")
read_file(requested)
```

### Shell injection — never pass user data through a shell

Any use of `exec`, `system`, `popen`, `subprocess`, or equivalent with a shell-interpolated
string containing external data is a critical vulnerability. The attacker can terminate your
command and run arbitrary commands instead.

```
# ❌ Shell interprets the full string — attackable
system("convert " + filename + " output.png")
exec(`ffmpeg -i ${input} out.mp4`)

# ✅ Pass arguments as a list/array — no shell involved
execFile("convert", [filename, "output.png"])
subprocess.run(["ffmpeg", "-i", input, "out.mp4"])
```

If the language and library support an array/list form that bypasses the shell — always use it.
Better still, use a native library for the operation instead of shelling out at all.

### Other injection classes — same principle, different context

- **Template injection**: never render user-supplied strings as templates
- **LDAP injection**: use parameterised LDAP queries or escape per RFC 4515
- **XML/XXE**: disable external entity processing in XML parsers; use JSON where possible
- **Deserialization**: never deserialise untrusted data with a general-purpose deserialiser
  (Java `ObjectInputStream`, Python `pickle`, Ruby `Marshal`) — use language-agnostic formats
  (JSON, Protobuf, MessagePack) with explicit schema validation instead

---

## 2. Authentication and Authorisation

### Authentication principles

- **Verify identity on every request** — never assume a session is still valid; re-check tokens
- **Use constant-time comparison** for all token and secret comparisons — standard `==` leaks
  timing information that can be used to forge valid tokens character by character
- **Pin token signing algorithms** — when using JWTs, always specify which algorithms are
  acceptable; leaving it unspecified allows algorithm confusion attacks (`none`, RS→HS swap)
- **Expire tokens** — sessions and tokens must have a finite lifetime; implement revocation
  for sensitive operations
- **Hash passwords with a purpose-built algorithm** — argon2id is the current recommendation;
  bcrypt and scrypt are acceptable; SHA-\* alone, MD5, and plain storage are never acceptable
  regardless of iteration count

### Authorisation principles

- **Check ownership on every operation** — verify that the authenticated user is allowed to
  act on the specific resource, not just that they are authenticated
- **Server-side always** — never rely on the client to restrict what it requests; any client
  can send any request regardless of what the UI shows
- **Principle of least privilege** — a service account, API key, or user session should have
  exactly the permissions it needs for its function, nothing more
- **Default deny** — if an authorisation check is missing or fails, the default outcome must
  be denial, not permission

```
# ❌ Checks authentication but not authorisation — any logged-in user can delete any file
delete_file(file_id)

# ✅ Ownership enforced in the data access layer
file = db.find(file_id, owner_id=current_user.id)  # returns nothing if not owned
if not file: reject(404)
delete_file(file.id)
```

### Session management

- Generate session IDs with a cryptographically secure random source (see section 4)
- Regenerate session IDs after privilege changes (login, role change)
- Invalidate server-side on logout — don't rely only on deleting the client cookie
- Set cookies with `HttpOnly`, `Secure`, and `SameSite=Strict` or `Lax`

---

## 3. Secrets and Credentials

### Never hardcode secrets

```
# ❌ Any form of hardcoded credential — will end up in version control
API_KEY = "sk-prod-abc123"
DB_PASSWORD = "hunter2"
SECRET = "my-secret-key"

# ✅ Read from the environment; fail loudly if missing
api_key = require_env("API_KEY")   # raise an error if not set, never silently use a default
```

This applies to: API keys, database passwords, JWT secrets, encryption keys, private keys,
OAuth client secrets, webhook signing secrets, internal service tokens — everything.

### Never log secrets

Before logging any object, request, config, or error:

- Explicitly list which fields are safe to log
- Redact or omit everything else

```
# ❌ Logs entire request body — may contain passwords, tokens, private data
log(request.body)
log(config)

# ✅ Log only known-safe fields
log({ user_id: request.body.user_id, action: request.body.action })
```

### Secrets in URLs — never

Query parameters and URL paths are logged by every proxy, CDN, browser history, and server
access log between the client and your service. Secrets must go in headers (Authorization,
X-API-Key) or in the request body, never in the URL.

### Secret scanning

Before committing, verify no secrets are present:

```bash
# git-secrets, truffleHog, or gitleaks — check what the project uses
git diff --staged | grep -iE "password|secret|api.?key|token|private.?key" | head -20
```

---

## 4. Cryptography

### Use standard implementations — never roll your own

Custom crypto is always broken. Use well-maintained, audited libraries that are standard in
the ecosystem. Check what the project already uses before adding a new crypto dependency.

### Algorithm selection

| Purpose               | Recommended                        | Never use                                |
| --------------------- | ---------------------------------- | ---------------------------------------- |
| Password hashing      | argon2id, bcrypt, scrypt           | MD5, SHA-\*, plain text, fast hashes     |
| Symmetric encryption  | AES-256-GCM                        | ECB mode (any cipher), RC4, DES, 3DES    |
| Asymmetric encryption | RSA-OAEP (≥2048-bit), X25519       | RSA-PKCS1v1.5, raw RSA                   |
| Digital signatures    | Ed25519, ECDSA P-256, RSA-PSS      | RSA-PKCS1v1.5                            |
| Data integrity / HMAC | HMAC-SHA256, HMAC-SHA512           | MD5, SHA-1, CRC32                        |
| Secure random tokens  | OS CSPRNG (see below)              | Math.random(), rand(), time-seeded PRNGs |
| Key derivation        | HKDF, PBKDF2 (≥600k rounds)        | Simple hash of password + salt           |
| TLS                   | TLS 1.2 minimum, TLS 1.3 preferred | SSL, TLS 1.0, TLS 1.1                    |

### Secure random number generation

Always use the operating system's cryptographically secure random source:

```
# The principle is the same across all languages:
# Use the OS/platform CSPRNG, never the standard math random

# ❌ Predictable — not cryptographic
token = random_string(32)        # backed by Math.random, rand(), time seed
id = uuid_v4_from_math_random()

# ✅ OS-backed CSPRNG — unpredictable, suitable for security tokens
token = os_csprng_bytes(32).hex()
# Node: crypto.randomBytes(32)
# Python: secrets.token_hex(32) or os.urandom(32)
# Rust: OsRng.fill_bytes(&mut buf)
# Go: crypto/rand.Read(buf)
# Ruby: SecureRandom.hex(32)
```

### Encryption correctness

- Always use **authenticated encryption** (AES-GCM, ChaCha20-Poly1305) — unauthenticated
  modes (AES-CBC, AES-CTR alone) allow attackers to tamper with ciphertext undetected
- Never reuse an IV/nonce with the same key — generate a fresh random IV for every encryption
- Store the IV alongside the ciphertext — it is not secret but must be unique
- Verify the authentication tag before using decrypted data — never decrypt-then-verify

---

## 5. HTTP and Network Security

### General principles (framework-agnostic)

- **Security headers**: set `Content-Security-Policy`, `X-Frame-Options: DENY`,
  `X-Content-Type-Options: nosniff`, `Strict-Transport-Security` on all responses
- **CORS**: restrict allowed origins to an explicit allowlist; never use wildcard (`*`) for
  endpoints that handle credentials or sensitive data
- **Rate limiting**: apply to all authentication endpoints, password reset, and any endpoint
  that is expensive or could be used for enumeration
- **HTTPS only**: redirect HTTP to HTTPS; use HSTS; never send sensitive data over plain HTTP
- **TLS configuration**: disable old TLS versions (1.0, 1.1); use strong cipher suites

### SSRF — validate URLs before fetching on behalf of a user

If your server fetches a URL supplied by a user or external input, an attacker can use it to
reach internal services, cloud metadata endpoints (169.254.169.254), and other restricted resources.

```
# ❌ Blind fetch of user-supplied URL
response = http.get(user_supplied_url)

# ✅ Validate against an allowlist before fetching
parsed = parse_url(user_supplied_url)
if parsed.hostname not in ALLOWED_HOSTS:
    reject("Host not allowed")
if parsed.scheme != "https":
    reject("HTTPS only")
response = http.get(parsed)
```

Block private IP ranges (`10.x`, `172.16-31.x`, `192.168.x`, `127.x`, `::1`) if the allowlist
approach is not feasible, but an explicit allowlist is always stronger.

### Mass assignment

Never apply the full body of an incoming request directly to a database record. An attacker
can include fields they should not be able to set (role, admin flag, owner ID, account balance).

```
# ❌ User can set any field on the record
db.update(record_id, request.body)

# ✅ Explicitly extract only the fields that are user-editable
db.update(record_id, {
    display_name: request.body.display_name,
    avatar_url: request.body.avatar_url,
})
```

---

## 6. File Handling

- **Validate file type from content**, not from the extension or `Content-Type` header —
  both are user-controlled. Read the first bytes (magic bytes / file signature) to detect the
  actual format.
- **Enforce size limits** before reading the file into memory
- **Generate a safe storage name** — never use the original filename for storage. Use a UUID or
  a hash of the content. Store the original name only in the database if needed for display.
- **Store outside the web root** — uploaded files must not be directly accessible via URL unless
  you explicitly intend to serve them, and then only after validation
- **Scan or process in isolation** — if the file will be processed (image resizing, document
  parsing), do it in a sandboxed environment if the format is complex

---

## 7. Dependency Security

Every dependency is part of your attack surface. Before adding one:

- Is it actively maintained?
- Does it have a known vulnerability disclosure process?
- Is the install count and community trust reasonable for what it does?
- Is there a lighter or already-present alternative?

After adding or updating dependencies, run the appropriate audit tool:

```bash
npm audit            # JavaScript / Node
cargo audit          # Rust
pip-audit            # Python
bundle audit         # Ruby
govulncheck ./...    # Go
dotnet list package --vulnerable   # .NET
```

Keep dependencies up to date. A vulnerability in a dependency is a vulnerability in your code.

---

## 8. Error Handling and Information Leakage

Errors sent to clients must never reveal:

- Stack traces
- Internal file paths or module names
- Database query structure or column names
- Framework or library versions
- Internal service names or IPs

```
# ❌ Full error detail exposed to caller
return { error: exception.message, trace: exception.stacktrace, query: failed_query }

# ✅ Log full detail internally; return a safe, generic message to the caller
log_internally(exception, request_context)
return { error: "An unexpected error occurred" }
```

In development environments it is acceptable to return more detail — but the mechanism to
do so must be explicitly gated on an environment flag, never on anything the caller controls.

---

## 9. Platform-Specific Considerations

When writing for a specific platform, check what secure storage and security APIs are
available before reaching for a generic solution:

- **Mobile (iOS/Android)**: use the platform Keychain/Keystore for sensitive data — never
  unencrypted local storage or shared preferences for tokens or credentials
- **Browser**: use `HttpOnly` cookies over `localStorage` for session tokens — JS-accessible
  storage is vulnerable to XSS
- **Server**: use secret management services (Vault, AWS Secrets Manager, GCP Secret Manager)
  rather than environment variables for production secrets where possible
- **CLI tools**: never write secrets to stdout, log files, or shell history; use secure prompts
  and clear sensitive variables from memory after use

---

## 10. Pre-Commit Security Checklist

Before considering code complete:

**Input and injection**

- [ ] All external input validated against strict type, format, and range rules
- [ ] All database queries use parameterised statements — no string interpolation
- [ ] All file paths resolved and verified to stay within an allowed base directory
- [ ] All shell commands use argument arrays — no user data in shell strings
- [ ] All user-supplied URLs validated against an allowlist before server-side fetch

**Auth and access control**

- [ ] Every operation that accesses or modifies data verifies ownership server-side
- [ ] All token comparisons use constant-time equality
- [ ] Passwords hashed with argon2id, bcrypt, or scrypt — never fast hashes
- [ ] Sessions regenerated on privilege changes; invalidated server-side on logout

**Secrets**

- [ ] No hardcoded credentials, keys, or tokens anywhere in the code
- [ ] Secrets loaded from environment; startup fails loudly if any are missing
- [ ] No secrets in log output, error responses, or URLs

**Cryptography**

- [ ] All random tokens generated from OS CSPRNG, not math random
- [ ] Encryption uses an authenticated mode (GCM, Poly1305)
- [ ] No custom crypto implementations

**HTTP**

- [ ] Security headers set on all responses
- [ ] CORS restricted to an explicit origin allowlist
- [ ] Rate limiting applied to auth and sensitive endpoints
- [ ] Error responses return only generic messages — no internal detail

**Dependencies**

- [ ] Vulnerability audit run and clean after any dependency change

---

## When You Spot a Vulnerability

If you find a security issue in existing code while working on something else — flag it
immediately, even if you were not asked to audit it:

```
Note: while working on X I noticed Y is vulnerable to Z
(e.g. unsanitised input passed directly to a shell command on line N of file).
I have not changed it as it is outside the current task, but it should be
addressed before this ships.
```

Never silently work around a vulnerability or leave it unflagged. Security debt compounds.
