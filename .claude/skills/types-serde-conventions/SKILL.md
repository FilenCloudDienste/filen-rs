---
name: types-serde-conventions
description: Conventions for adding or changing types in filen-types / filen-sdk-rs — the permissive_u64 serde helpers and their two gotchas, and the #[js_type] macro arguments including the externally-managed-serde pattern. Read before adding a field to an API type, adding a new API type, or touching a type that carries #[js_type].
---

# Type & serde conventions

## `permissive_u64` — every deserialized `u64`

`filen-types/src/serde/number.rs` defines `permissive_u64`, which deserializes a `u64` from
a JSON integer, float, **or** string (via `crate::conversions::{f64_to_u64, str_to_u64}`);
serialization just forwards to the default. The API is inconsistent about number encoding,
so this is not optional politeness — it is what keeps responses parsing.

**Convention:** every **deserialized** `u64` field in `filen-types` (i.e. in a type deriving
`Deserialize`, or carrying `#[js_type]` without `no_deser`) gets:

```rust
#[serde(with = "crate::serde::number::permissive_u64")]
pub size: u64,
```

Two gotchas:

1. **`Option<u64>` needs `default` as well:**

   ```rust
   #[serde(default, with = "crate::serde::number::permissive_u64_opt")]
   pub timestamp: Option<u64>,
   ```

   A custom `deserialize_with` disables serde's automatic "missing field → `None`" for
   `Option`, so **without `default` a missing field is a hard error**.

2. **Do not swap `crate::serde::option::default` for `permissive_u64_opt` on outgoing
   `Request` types.** `option::default::serialize` emits `None → 0` (`T::default()`), which
   the wire format relies on; `permissive_u64_opt` emits `None → null`. Example: `v3/dir/size`
   `Request.{sharer_id, receiver_id}` keep `option::default`. Outgoing requests do not need
   permissive *de*serialization anyway.

**Scope rule:** apply only to types that derive `Deserialize`. Serialize-only types (e.g. the
hand-written `Serialize` impl in `shared/out_root.rs`) are skipped.

---

## `#[js_type]` — one type, three platforms

`filen-macros`' `#[js_type]` generates platform-specific derives so a single type serves:

- **Native** — plain struct/enum with `Debug, Clone, PartialEq, Eq`
- **WASM** — `tsify::Tsify` + serde + tsify ABI annotations, all under `#[cfg_attr(<wasm cond>, …)]`
- **UniFFI** — `uniffi::Record` (structs) / `uniffi::Enum` (enums) under
  `#[cfg_attr(feature = "uniffi", …)]`

| Argument | Effect |
|----------|--------|
| `import` | `tsify(from_wasm_abi)` — type can be passed *into* WASM |
| `export` | `tsify(into_wasm_abi, large_number_types_as_bigints, hashmap_as_object)` — passed *out* |
| `wasm_all` | WASM condition becomes `all(target_family = "wasm", target_os = "unknown")` (default also requires `feature = "wasm-full"`) |
| `wasm_worker` | WASM condition includes `feature = "wasm-worker"` |
| `no_ser` / `no_deser` | Suppress the `derive(serde::Serialize)` / `Deserialize` inside the cfg_attr — use when serde is provided unconditionally |
| `no_default` | Suppress the default `#[derive(Debug, Clone, PartialEq, Eq)]` |
| `tagged` | Force tagged enum mode |

Key invariant: `no_ser` suppresses only the serde derive, **not** `tsify(into_wasm_abi, …)`.
That is what makes the pattern below work.

### Externally managed serde

When a type needs `Serialize`/`Deserialize` on **all** platforms, not just WASM, derive them
unconditionally and tell the macro to stay out of it:

```rust
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[js_type(import, export, wasm_all, no_ser, no_deser)]
pub struct MyType { … }
```

Canonical example: `StringifiedClient` in `filen-sdk-rs/src/auth/mod.rs`.

Common mistakes:

1. Adding an unconditional `serde(rename_all = "camelCase")` **without** `no_ser, no_deser`
   — it duplicates the macro's own attribute on WASM. (With both flags set, the macro omits
   it, which is why the pattern above is consistent.)
2. Leaving a field-level `serde(default)` inside a wasm `cfg_attr` when serde is
   unconditional — the field attribute must be unconditional too.
3. Writing a parallel "serializable" workaround type (e.g. a hand-rolled
   `SerializableClientConfig`). Never needed — `no_ser, no_deser` plus unconditional derives
   is the supported route.

### Tagged variants

If struct fields carry `#[js_type(tagged)]`, the macro emits a companion `{Name}Tagged`
struct for WASM with those field types replaced by their tagged equivalents, gated on the
wasm condition; UniFFI gets a `type {Name}Tagged = {Name}` alias. For enums with `export`,
the macro emits a `{Name}Tagged` enum for WASM serialization while the main enum takes the
`uniffi::Enum` derive.
