---
name: filen-api-behavior
description: Server-side Filen API semantics that are NOT visible from this repo's code — same-name upload versioning, case-insensitive name-hash dedup, and which uuid the dir-link endpoints are keyed by. Read before writing or reviewing upload, rename, collision-handling, or public-link code.
---

# Filen API behaviour (server-side semantics)

These are facts about the **server**, confirmed against the backend or the official
`filen-sdk-ts`. They cannot be derived by reading this repo — the request types look
symmetric where the server behaviour is not.

---

## 1. Uploading a same-name file replaces (and may version) the old one

When a file is uploaded into a directory that already holds a file with the same name
(same `parent` + `name_hashed`), **the server replaces it**: the new upload becomes the
current file at that name and the old uuid stops being current. With account-level
versioning **enabled**, the old uuid survives as a prior version — it still resolves via
`v3/file`, byte-identical, parent unchanged, and is *not* trashed. With versioning disabled
the version chain does not grow. Both halves are pinned by tests
(`file_tests::get_replaced_file`, `user_tests` for the disabled case), so check
`set_versioning_enabled` state before reasoning about history.

- The client always mints a fresh `Uuid::new_v4()`
  (`FileBuilderOptionalName::new`, `filen-sdk-rs/src/fs/file/mod.rs`) and sends **no**
  overwrite or replace flag — the upload `empty`/`done` requests in `filen-types` carry no
  such field (their `version` field is the *encryption* version, not a file-version
  selector). The server infers versioning from `(parent, name_hashed)`.
- History is readable via `Client::list_file_versions(&RemoteFile)`.

**Consequence:** a re-upload of a changed same-name file does **not** need to delete or
trash the old uuid to avoid duplicates. Only explicit deletions need handling. Writing
cleanup code for this is not just redundant, it can destroy version history.

---

## 2. Name dedup is case-insensitive and keyed on a client-supplied hash

The server dedups items under the same parent **case-insensitively**, keyed on a
**client-supplied name hash** — not on the name itself, which is encrypted and unreadable
to the server.

- The hash is computed over the **lowercased** name:
  - V2: SHA512 → hex → SHA1 of `name.to_lowercase()` — `auth/v2.rs` `hash_name` +
    `crypto/v2.rs` `hash`
  - V3: HMAC-SHA256 over `name.to_lowercase()` — `auth/v3.rs` `hash_name`
  - Dispatched by `Client::hash_name` (`auth/mod.rs`) on the auth version.
- Sent as `name_hashed` in `dir/create`, file upload, `upload/empty`, `dir/exists`. It is a
  plain string with **no validation** server-side or type-side.
- A conforming client always derives `name_hashed` from the real name, so `Note` and `note`
  hash identically and only one survives.

**Consequence:** a non-conforming or buggy client that sends a `name_hashed` not matching
its lowercased name **bypasses the dedup**. Two items under one parent whose decrypted
names differ only in case *can* exist on a real drive. Any code that assumes
case-folded names are unique per parent must handle the collision rather than assume it
away — this was confirmed live, not reasoned about, using a `malformed`-gated seam that
sends a chosen name hash. (On `main` the `malformed` feature exposes
`Client::create_malformed_dir`, which fakes the *metadata*; the name-hash variant
`create_dir_with_name_hash` currently exists only on the unmerged `sync-engine` branch.)

Filen is also moving toward case-insensitive names generally, so collision detection should
normalise with NFC + case-fold, not raw byte equality.

---

## 3. Dir-link endpoints are keyed by the *directory* uuid

`v3/dir/link/remove`, `dir/link/edit` and `dir/link/status` all identify the public link by
the **linked directory's uuid**, not by the link's own `linkUUID`. Only `dir/link/add` and
`dir/link/info` take the `linkUUID`.

Passing the link uuid to `remove` returns `public_link_not_found`. This is why
`Client::remove_dir_link` (`filen-sdk-rs/src/connect/mod.rs`) takes a
`&RemoteDirectory` rather than a link — matching `filen-sdk-ts`'s
`dir().link().remove({ uuid: itemUUID })`. Covered by `dir_public_link_remove` in
`tests/connect_tests.rs`.

Note the asymmetry: **file** link removal genuinely needs the link uuid + salt, because it
goes through `file/link/edit` with a Disable action rather than a dedicated endpoint.
