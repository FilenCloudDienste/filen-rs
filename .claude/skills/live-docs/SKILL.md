---
name: live-docs
description: CRITICAL: Always use this skill, no matter what task you are working on!
---

# Live Documentation Lookup

Training data goes stale. APIs change. Config options get renamed. Major versions break things.
Before writing code that depends on any external library or API, fetch the current docs.
This is not optional. One stale API call can silently break production.

---

## Step 1 — Identify the ecosystem and installed versions

Before fetching anything, identify the language, package manager, and exact versions in the project:

### JavaScript / TypeScript

```bash
cat package.json | grep -E '"dependencies"|"devDependencies"' -A 1000 | head -1000
# Exact resolved versions:
cat package-lock.json 2>/dev/null | grep -A 2 '"<package>"'
cat bun.lockb 2>/dev/null | strings | grep "^<package>@"
cat yarn.lock 2>/dev/null | grep "^<package>@"
```

### Python

```bash
cat requirements.txt 2>/dev/null
cat pyproject.toml 2>/dev/null
cat Pipfile 2>/dev/null
pip show <package>  # exact installed version
```

### Rust

```bash
cat Cargo.toml | grep -A 50 '\[dependencies\]'
cat Cargo.lock | grep -A 2 'name = "<crate>"'
```

### Go

```bash
cat go.mod
cat go.sum | grep "<module>"
```

### Ruby

```bash
cat Gemfile
cat Gemfile.lock | grep "    <gem>"
```

### PHP

```bash
cat composer.json | grep -A 30 '"require"'
cat composer.lock | grep -A 3 '"name": "<package>"'
```

### Swift / iOS

```bash
cat Package.swift 2>/dev/null
cat Podfile 2>/dev/null
cat Podfile.lock 2>/dev/null | grep "<pod>"
```

### Dart / Flutter

```bash
cat pubspec.yaml
cat pubspec.lock | grep -A 2 '<package>:'
```

### .NET / C#

```bash
cat *.csproj | grep "PackageReference"
```

### Java / Kotlin

```bash
cat build.gradle | grep -E "implementation|api|compileOnly"
cat pom.xml | grep -A 3 '<dependency>'
```

**The version number matters.** The same library at v1 and v2 can have completely different APIs. Always look up docs for the **exact installed version**, not just "latest."

---

## Step 2 — Find the right documentation source

### By package registry (universal fallback)

| Ecosystem        | Registry / Index                                | Typical doc location     |
| ---------------- | ----------------------------------------------- | ------------------------ |
| npm (JS/TS)      | `npmjs.com/package/<n>`                         | linked from package page |
| PyPI (Python)    | `pypi.org/project/<n>`                          | linked from project page |
| crates.io (Rust) | `crates.io/crates/<n>`                          | `docs.rs/<n>`            |
| pkg.go.dev (Go)  | `pkg.go.dev/<module>`                           | inline on pkg.go.dev     |
| RubyGems         | `rubygems.org/gems/<n>`                         | linked from gem page     |
| Packagist (PHP)  | `packagist.org/packages/<vendor>/<n>`           | linked to GitHub/docs    |
| pub.dev (Dart)   | `pub.dev/packages/<n>`                          | inline on pub.dev        |
| NuGet (.NET)     | `nuget.org/packages/<n>`                        | linked from package page |
| Maven Central    | `mvnrepository.com/artifact/<group>/<artifact>` | —                        |
| Hex (Elixir)     | `hex.pm/packages/<n>`                           | `hexdocs.pm/<n>`         |

### Finding version-pinned docs

Most documentation sites support version-pinned URLs. Look for a version selector on the docs site, or check:

- `https://docs.rs/<crate>/<version>/` — Rust, always version-pinned
- `https://pkg.go.dev/<module>@<version>` — Go
- `https://docs.python.org/<major>.<minor>/` — Python stdlib
- GitHub releases/tags: `https://github.com/<org>/<repo>/tree/v<version>`
- GitHub changelog: `https://github.com/<org>/<repo>/releases`

When version-pinned docs aren't available, always verify the docs version matches what's installed.

---

## Step 3 — Fetch before you write

Use web search and web fetch to retrieve current docs. Do this **before** writing any code — not after.

### Search strategy

Be specific. Include the library name, version, and the exact API or concept:

```
django 5.0 middleware configuration
sqlalchemy 2.0 async session
tokio 1.x spawn_blocking
axum 0.7 router nested
flutter 3.x navigator 2.0
rails 7.1 turbo streams
spring boot 3 autoconfiguration
openai python sdk v1 chat completions
```

When unsure of the exact version, search for the changelog or migration guide first:

```
<library> migration guide v2 to v3
<library> breaking changes <year>
<library> changelog
```

### Fetch strategy

Fetch the specific page for what you're using, not just the homepage:

```
# Too broad
https://docs.djangoproject.com/

# Right level — specific topic
https://docs.djangoproject.com/en/5.0/topics/http/middleware/

# For breaking changes
https://docs.djangoproject.com/en/5.0/releases/5.0/

# GitHub releases when official docs don't cover it
https://github.com/django/django/releases
```

For Rust crates, `docs.rs` always has version-specific auto-generated API docs — prefer it over README:

```
https://docs.rs/<crate>/<version>/<crate>/struct.<StructName>.html
```

For Go, `pkg.go.dev` has per-version docs with full API reference:

```
https://pkg.go.dev/<module>@<version>#section-documentation
```

### What to look for in any docs

- **Function / method signatures** — parameter names, types, order, required vs optional
- **Return types** — especially for async or error-returning APIs
- **Breaking changes since the version you know** — renamed params, removed methods, changed defaults
- **Required setup** — imports, initialization, config files, environment variables, feature flags
- **Platform / runtime constraints** — OS support, minimum language version, incompatible combinations
- **Deprecations** — the old way may still compile but the new way is correct
- **Examples** — official examples beat inferred usage every time

---

## Step 4 — Apply what you found, not what you remember

After fetching:

1. **Use the exact signatures from the docs** — don't interpolate or guess missing parameters
2. **Match the version** — if the docs are for v3 but the project has v2, find v2 docs specifically
3. **Check required setup steps** — many libraries need initialization, registration, or config that isn't obvious from the API alone
4. **Flag outdated versions** — if the installed version is significantly behind current, mention it; don't silently write against an EOL API
5. **Note platform constraints** — if a feature is Linux-only, async-only, or requires a specific runtime version, say so

---

## Always fetch docs for these categories — they change constantly

Regardless of language or ecosystem, these categories have high churn and training data is frequently wrong or incomplete:

- **AI / LLM SDKs** — OpenAI, Anthropic, Google Gemini, LangChain, LlamaIndex — models, parameters, and APIs change frequently
- **Cloud provider SDKs** — AWS, GCP, Azure — service APIs, auth flows, and SDK versions evolve constantly
- **Mobile platform APIs** — iOS SDK, Android SDK, Flutter — deprecations across OS versions
- **Web framework routing and middleware** — Next.js App Router, Rails, Django, Laravel, ASP.NET — major versions break conventions
- **ORM query APIs** — SQLAlchemy 2.0, Prisma, Drizzle, ActiveRecord, GORM — query syntax changes significantly between majors
- **Authentication libraries** — NextAuth, Devise, Passport, OAuth flows — security-driven changes
- **Build tools** — Webpack, Vite, Turbopack, Cargo, Gradle, Bazel — config format changes across versions
- **Database drivers** — especially async drivers (asyncpg, sqlx, go-sql) — connection pool APIs evolve
- **Container / infrastructure tooling** — Docker, Kubernetes, Terraform — resource specs and API versions change
- **Any library that recently released a major version** — assume breaking changes until docs confirm otherwise

---

## When docs are unavailable or behind a login

If a fetch fails, returns a login wall, or gives unhelpful results:

1. **GitHub README**: `https://github.com/<org>/<repo>#readme`
2. **GitHub releases**: `https://github.com/<org>/<repo>/releases`
3. **Search with site filter**: `<library> <function> example site:github.com`
4. **Source code itself**: read the type signatures, docstrings, or comments directly from the installed source or the repo — always authoritative
5. **If still unverifiable** — tell the developer and leave a clear comment in the code:

```python
# NOTE: Could not verify current API for X in <library> v<version>.
# Confirm against docs before shipping: <best URL you found>
```

---

## What NOT to do

- **Don't skip lookup because you're confident** — confidence is how stale knowledge ships
- **Don't fetch the homepage and call it done** — fetch the specific API page you're using
- **Don't trust the first search result** — verify it documents the version actually installed
- **Don't write code first and look up docs to confirm** — look up first, write after
- **Don't assume defaults are stable across major versions** — default values and behaviors change
- **Don't assume the same library works the same in all languages** — `redis-py`, `ioredis`, and `go-redis` have different APIs for the same Redis commands
