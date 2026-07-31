---
name: mobile-build
description: How to build this Rust code into the filen-ts mobile apps for on-device testing — how each of the two consumption paths is wired (Drive cache via git submodule, @filen/sdk-rs via published npm package), ubrn Android/iOS commands, the debug-vs-release performance trap, and the stale-xcframework and prebuild gotchas. Read before building, running, or profiling the mobile app against local SDK changes.
---

# Building filen-rs into the mobile apps

## The two consumption paths (get this wrong and you test stale code)

The mobile app is the `filen-ts` repo, package `packages/filen-mobile`. **Neither path
picks up your working clone by default.**

| What the app uses | Built from | How it's wired |
|-------------------|-----------|----------------|
| `filen-mobile-native-cache` — the **Drive** cache | `packages/filen-mobile/filen-rs` | a real **git submodule**: a separate checkout of this repo, normally detached at the recorded commit |
| `@filen/sdk-rs` — `filen-sdk-rs` via `ubrn`, powering **Notes/Chats + transfers** | the published npm package | an ordinary pinned dependency in `packages/filen-mobile/package.json` |

**Drive cache changes** must reach the submodule checkout — pushing to GitHub is not
required, you can fetch straight from your clone:

```bash
sub=<filen-ts>/packages/filen-mobile/filen-rs
git -C "$sub" fetch <your-filen-rs-clone> 'branch:branch'
git -C "$sub" checkout branch
```

Landing it means a submodule pointer bump plus a `filen-ts` superproject commit.

**SDK (Notes/Chats/transfers) changes** are *not* picked up by rebuilding
`filen-sdk-rs/web`: the app installs a published version of `@filen/sdk-rs`. Check what you
actually have before trusting any on-device test:

```bash
readlink <filen-ts>/packages/filen-mobile/node_modules/@filen/sdk-rs   # empty = the npm package, not your build
```

### Pointing the app at your local `filen-sdk-rs/web`

Symlink it — smallest change, and it touches no tracked file:

```bash
rs=<your-filen-rs-clone>                 # the clone you are editing
cd <filen-ts>/packages/filen-mobile

# 1. the link target must be a *generated* package: the react-native entry point is
#    ./src/index.tsx, and src/ android/ ios/ cpp/ are gitignored build output
(cd "$rs/filen-sdk-rs/web" && npm install && npx ubrn build android --and-generate)

# 2. replace the installed package with a link to it
ln -sfn "$rs/filen-sdk-rs/web" node_modules/@filen/sdk-rs
readlink node_modules/@filen/sdk-rs      # should print your path

# 3. rebuild natively — Metro reload does not pick up a new .so
npx expo run:android
```

**The symlink alone is not enough** — Metro follows it to the real path and then resolves
that file's own imports from *there*, so bundling dies with:

```
Unable to resolve module react-native from <your-clone>/filen-sdk-rs/web/src/NativeSdkRs.ts
```

Point Metro at the link target and pin module lookup to the app's `node_modules`, in
`packages/filen-mobile/metro.config.js`:

```js
const path = require("path")
const localSdkRs = "<your-clone>/filen-sdk-rs/web"

const config = {
	...defaultConfig,
	watchFolders: [localSdkRs],
	resolver: {
		...defaultConfig.resolver,
		nodeModulesPaths: [path.resolve(__dirname, "node_modules")],
		extraNodeModules: { /* the existing entries */ }
	}
}
```

Verified 2026-07-31: without those two lines `npx expo export --platform android` fails on
the `react-native` resolution above; with them it exports cleanly through the symlink. Keep
the edit local — it hardcodes your path, so don't commit it.

Caveats:

- **Any `npm install` / `npm ci` in `packages/filen-mobile` silently restores the published
  tarball and destroys the link.** Re-run the `ln`, and re-run the `readlink` check before
  believing a test result. This is the failure mode to suspect when a change you *know* you
  built does not show up on device.
- Metro (0.84 here) resolves symlinks, but if it refuses to serve files from outside the
  project root, add the target to `watchFolders` in `metro.config.js`.
- Alternative that survives installs: `npm install "file:$rs/filen-sdk-rs/web"` — npm links
  local `file:` dependencies rather than copying them. It records the absolute path in
  `package.json` and `package-lock.json`, so it dirties tracked files; never commit that.
- Undo with `npm install @filen/sdk-rs` (or plain `npm install` if you used the link), which
  puts the pinned published version back.

How each platform builds the cache during `expo prebuild` (both run cargo with cwd
`<projectRoot>/filen-rs`, i.e. the submodule; both are **release** builds with
`-F heif-decoder`, configured in `app.config.ts`):

- Android — `plugins/withAndroidRustBuild.ts`:
  `cargo ndk -t x86_64 -t arm64-v8a build --release -p filen-mobile-native-cache -F heif-decoder`,
  then `uniffi-bindgen` for the Kotlin bindings and a copy of the `.so` into `jniLibs`.
  Needs **cargo-ndk 4.x** (`heif-decoder`'s `build.rs` reads `ANDROID_ABI`, which 3.5.4
  does not set).
- iOS — `plugins/withFileProvider.ts`:
  `cargo build --lib --release --target aarch64-apple-ios --target aarch64-apple-ios-sim
  -p filen-mobile-native-cache -F heif-decoder`, then `uniffi-bindgen-swift` and
  `xcodebuild -create-xcframework`.

---

## Android (`@filen/sdk-rs` via ubrn)

Run in `filen-sdk-rs/web/`:

```bash
# only if you have several NDKs installed — CI pins this version (.github/workflows/ci.yml)
export ANDROID_NDK_HOME=$ANDROID_HOME/ndk/27.0.12077973
npm run ubrn:android          # both ABIs (arm64-v8a + x86_64), matches CI debug
npm run ubrn:android:arm      # single ABI, ~2x faster
npm run ubrn:android:x86
```

Pick the device ABI with `adb shell getprop ro.product.cpu.abi`. No `wasm-pack` needed —
the wasm bundle is browser-only; React Native resolves to `src/index.tsx` + the ubrn
native lib.

**Debug vs release matters enormously.** All three `ubrn:android*` scripts build **debug**
(no `--release`, so `opt-level=0`) — one measurement on an arm64 debug build gave a ~720 MB
unoptimised `libfilen_sdk_rs.so` against ~30 MB for release. Debug AES-GCM/rustls is
roughly **10–50× slower** — fine for functional testing, worthless and actively misleading
for performance work (a debug build alone can peg a core during a download).

```bash
# perf-representative build
./node_modules/.bin/ubrn build android --release -t arm64-v8a --and-generate
npm run ubrn:release                       # all platforms/targets, release

# profiling build: release codegen + DWARF, no file edits
CARGO_PROFILE_RELEASE_DEBUG=true ./node_modules/.bin/ubrn build android --release -t arm64-v8a --and-generate
```

After building, relink into the app with `npx expo run:android` from the mobile dir — a
Metro/JS reload does **not** pick up a new `.so`.

Slow part is `heif-decoder`'s C++ rebuild every time (incremental broken, cmake-rs bug).
Build config: `filen-sdk-rs/web/ubrn.config.yaml` (features
`uniffi,heif-decoder,http-provider,cache`); `ubrn` accepts `--config <file>` and `-t <abi>`.

---

## iOS (simulator)

```bash
cd <filen-ts>/packages/filen-mobile
npx expo prebuild --platform ios   # builds the cache xcframework AND copies the
                                   # file-provider Swift out of the submodule
npm run ios                        # xcodebuild + install/launch
```

**Re-run prebuild after every file-provider Swift edit** — `npm run ios` alone will not
re-copy, because `ios/` already exists so prebuild is skipped. Metro on :8081 is shared
with Android.

### Gotchas

- **Stale `FilenSdkRsFramework.xcframework`** — relevant only when the app is pointed at
  your local `filen-sdk-rs/web` rather than the published package. The xcframework and
  `SdkRs.podspec` there are **generated and gitignored** (as are `src/`, `android/`, `ios/`,
  `cpp/`): a fresh clone has none of them until an `ubrn build … --and-generate` run. After
  an SDK API change, a stale xcframework fails to link with a single missing
  `_uniffi_filen_sdk_rs_checksum_*` symbol. Fix:
  `cd filen-sdk-rs/web && npx ubrn build ios --release --and-generate`. No `pod install`
  needed when the pod resolves through the local dev path.
- **Debug iOS *device* builds can fail to link** with `Undefined symbols: ___chkstk_darwin`
  from blake3's NEON object — seen on Xcode 26.6 / SDK 26.5 with a bare
  `cargo build --target aarch64-apple-ios` (and `npm run ubrn:ios`, also debug). Diagnosed
  as a deployment-target mismatch: rustc's bare default for `aarch64-apple-ios` is far older
  than the SDK minimum the cc-compiled C objects use, and at `-O0` those emit the stack-probe
  intrinsic. Not a code problem, and it does not affect the real flows (the ubrn xcframework
  and expo prebuild both build `--release`); if you hit it in a debug build, build release or
  set `IPHONEOS_DEPLOYMENT_TARGET` explicitly. CI is unaffected — its iOS job is clippy-only
  and never links.
- Xcode `nm` reporting `Unknown attribute kind (105)` on the `.a` is **cosmetic** (rust LLVM
  vs Xcode LLVM); `ld` links fine. Check symbol presence with
  `grep -a -c <symbol> lib.a` instead.
- `CoreSimulator is out of date` after an Xcode update → `sudo xcodebuild -runFirstLaunch`.

### Standalone harnesses (faster iteration than a full app build)

Both are submodules of `filen-ts` under `packages/filen-mobile/`, and both expect a
`filen-rs` checkout as their sibling (`../filen-rs`) — which is exactly the `filen-rs`
submodule next to them.

- **iOS**: `filen-ios-file-provider/FilenFileProvider.xcodeproj`, with
  `./build-rust.sh --rust-dir ../filen-rs --rust-crate-name filen-mobile-native-cache`
  (builds release for `aarch64-apple-ios-sim` + `aarch64-apple-ios`). Credentials come from
  a gitignored `secrets.xcconfig` referenced by the xcodeproj; the values reach the app as
  `Info.plist` `$(VAR)` substitutions read via `Bundle.main.infoDictionary` — `EMAIL`,
  `MASTER_KEYS`, `PRIVATE_KEY`, `API_KEY`, `AUTH_VERSION`, `BASE_FOLDER_UUID`. The readme
  still tells you to copy `.env.example` to `.env`; that instruction is stale.
- **Android**: `filen-android-documents-provider` — its `app/build.gradle` defines
  `rustDirPath = "${projectDir}/../../filen-rs"` and drives the cargo build itself. (This
  Gradle path is the *standalone* harness only; the expo app builds via the prebuild plugin
  described above.)
