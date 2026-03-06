# API Migration Propagator Memory

See `patterns.md` for detailed crate architecture notes.

## Key Facts
- Workspace uses nightly Rust (`nightly-2026-02-20`)
- `cargo check --workspace --exclude heif-decoder` is the standard check command
- `filen-mobile-native-cache` uses UniFFI; serde derives only apply under specific cfg guards
- `#[js_type(import, export, wasm_all)]` macro adds Serialize/Deserialize only under WASM, NOT on native

## Common Migration Patterns
- When SDK types lose Serialize/Deserialize on native: create a local mirror struct with full serde derives in the dependent crate and convert via `From`/`Into`
- `UnsharedFSObject` → `NonRootFileType<'_, Normal>`: same variant names (Root/Dir/File), same inner Cow types for Normal category
- `list_dir` now takes `&DirType<'_, Cat>` (= `&UnsharedDirectoryType<'_>` for Normal) + `Option<&F>` progress callback — second arg is `None::<&fn(u64, Option<u64>)>` when unused
- `set_favorite(&mut NonRootItemType)` replaced by `set_dir_favorite(&mut RemoteDirectory)` / `set_file_favorite(&mut RemoteFile)`
- `list_trash` is now a dedicated `client.list_trash(progress)` method, not a `list_dir` call with `ParentUuid::Trash`

## Category System (post a00c7af refactor)
- `Normal::Dir = RemoteDirectory`, `Normal::File = RemoteFile`, `Normal::Root = RootDirectory`
- `NonRootFileType<'a, Cat>` = {Root(Cow<Cat::Root>), Dir(Cow<Cat::Dir>), File(Cow<Cat::File>)}
- `DirType<'a, Cat>` = {Root(Cow<Cat::Root>), Dir(Cow<Cat::Dir>)} — used for list_dir
- `UnsharedDirectoryType<'a>` = `DirType<'a, Normal>` (type alias)
