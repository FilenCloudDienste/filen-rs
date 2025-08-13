<br/>
<p align="center">
  <h3 align="center">Filen Rust</h3>

  <p align="center">
	Rust crates for use with Filen services
	<br/>
	<br/>
  </p>
</p>

![Contributors](https://img.shields.io/github/contributors/FilenCloudDienste/filen-rs?color=dark-green) ![Forks](https://img.shields.io/github/forks/FilenCloudDienste/filen-rs?style=social) ![Stargazers](https://img.shields.io/github/stars/FilenCloudDienste/filen-rs?style=social) ![Issues](https://img.shields.io/github/issues/FilenCloudDienste/filen-rs) ![License](https://img.shields.io/github/license/FilenCloudDienste/filen-rs)

A comprehensive Rust implementation of Filen's cloud storage services, providing secure file operations, mobile platform bindings, and advanced features like HEIF image processing. This monorepo contains multiple crates designed for different use cases, from core SDK functionality to mobile app integration.

**⚠️ Development Notice:** Everything here is being actively developed and is likely to undergo multiple refactors before a more public release.

## 🚀 Quick Start

```bash
# Build the entire workspace
cargo build

# Run all tests
cargo test
```

## 📦 Crates Overview

### Core Libraries

#### 🔧 `filen-sdk-rs`

The primary SDK for interacting with Filen's cloud storage services. This is a Rust translation of the existing [filen-sdk-ts](https://github.com/FilenCloudDienste/filen-sdk-ts) repository, providing:

- **Authentication** - Multi-version auth support (v1, v2, v3) with secure credential handling
- **File Operations** - Upload, download, encryption/decryption with AES-GCM and RSA
- **Directory Management** - Create, list, move, delete operations with hierarchical structure
- **Sync & Locking** - Drive-level locking and synchronization mechanisms
- **Search** - Search file and directory names
- **Sharing** - File and directory sharing and creation of public links

**Status**: Partial implementation - missing missing some features from the TypeScript SDK.

#### 📋 `filen-types`

Shared type definitions and serialization utilities used across all crates:

- **API Types** - Request/response structures for all Filen API endpoints
- **Authentication Types** - User credentials, tokens, and auth state
- **File System Types** - File/directory metadata, permissions, and hierarchy
- **Custom Serialization** - Custom serde functions for unique server API values

#### 🔐 `filen-macros`

Procedural macros providing development utilities:

- **Test Macros** - `shared_test_runtime` for consistent async test execution
- **Code Generation** - Utilities for reducing boilerplate across the codebase

### Mobile & Platform Integration

#### 📱 `filen-mobile-native-cache`

Native caching layer with UniFFI bindings for mobile platforms:

- **SQLite Integration** - Embedded database with optimized queries for file metadata
- **UniFFI Bindings** - Auto-generated Kotlin/Swift interfaces for mobile apps
- **Platform Logging** - iOS (oslog) and Android (android_log) native logging
- **Progress Tracking** - Upload/download progress callbacks for UI integration
- **Local Sync** - Efficient and robust synchronization between local cache and remote state

**Mobile Platform Support**:

- **Android**: Used by [filen-android-documents-provider](https://github.com/FilenCloudDienste/filen-android-documents-provider)
- **iOS**: Used by [filen-ios-file-provider](https://github.com/FilenCloudDienste/filen-ios-file-provider)

**Future Plans**: Expected to be split into separate `filen-native-cache` and `filen-mobile` crates.

### Specialized Components

#### 🖼️ `heif-decoder`

HEIF/HEIC image format decoder with native library compilation:

- **HEIF Support** - Decode HEIF/HEIC images using libheif
- **Source Compilation** - Builds libheif and libde265 from source (git submodules in `deps/`)
- **Image Integration** - Compatible with the Rust `image` crate ecosystem

#### 🧪 `test-utils`

Shared testing infrastructure for integration tests:

- **Test Resources** - Managed test environments with automatic cleanup
- **Authentication** - Test account management with environment variable configuration
- **Async Runtime** - Shared Tokio runtime for consistent test execution
- **Random Data** - Utilities for generating test files and content

#### 🔗 `uniffi-bindgen` & `uniffi-bindgen-swift`

Minimal wrapper crates for UniFFI binding generation:

- **Kotlin Bindings** - Generate JNI interfaces for Android integration
- **Swift Bindings** - Generate Swift interfaces for iOS integration
- **Build Integration** - Required due to UniFFI limitations for workspace builds

## 🏗️ Architecture

### Data Flow

1. **Mobile Apps** (iOS/Android) ↔ **filen-mobile-native-cache** (UniFFI)
2. **Native Cache** ↔ **filen-sdk-rs** (Core API)
3. **SDK** ↔ **filen-types** (Shared Types)
4. **SDK** ↔ **Filen Backend** (HTTPS/JSON)

### Security Model

- **End to End Encryption**: Files encrypted client-side before upload
- **Version Support**: Backward compatibility with multiple encryption versions
- **Zero-knowledge**: Server cannot access file contents or metadata

### Database Schema

The native cache uses SQLite with embedded SQL queries for:

- File/directory metadata caching
- Search indexing and full-text search
- Recent files tracking
- Favorites and user preferences
- Sync state management

## 🔨 Development

### Prerequisites

- Rust 1.89+ (uses 2024 edition)
- CMake (for heif-decoder native builds)
- Git (for submodule dependencies)

### Building

```bash
# Clone with submodules for HEIF decoder
git clone --recursive https://github.com/FilenCloudDienste/filen-rs.git

# Build all crates
cargo build

# Build with HEIF decoder support
cargo build --features heif-decoder
```

Incremental builds for heif-decoder are broken due to a bug in cmake-rs, see [this](https://github.com/rust-lang/cmake-rs/issues/248) issue

### Testing

Integration tests require environment variables for test accounts.
These can be placed either in a .env file or directly in the environment:

```bash
# Required environment variables
export TEST_EMAIL="test@example.com"
export TEST_PASSWORD="password"

# For sharing tests
export TEST_SHARE_EMAIL="share@example.com"
export TEST_SHARE_PASSWORD="password"

# Run all tests
cargo test

# Run specific crate tests
cargo test -p filen-sdk-rs
cargo test -p filen-mobile-native-cache
```

### Mobile Development

For mobile specific builds, see the relevant repositories ([ios](https://github.com/FilenCloudDienste/filen-ios-file-provider), [android](https://github.com/FilenCloudDienste/filen-android-documents-provider))
The `filen-mobile-native-cache` crate generates platform-specific bindings:

### Compatibility Status

- ✅ **Authentication**: All auth versions supported
- ✅ **File Operations**: Core upload/download functionality
- ✅ **Directory Management**: Full hierarchy support
- ✅ **Mobile Bindings**: UniFFI integration complete
- ⚠️ **Feature Parity**: Some advanced features still in development
    - Notes
    - Chats
    - Sockets
    - User Settings
    - Health
    - Contact Blocking

## 📚 Documentation

- **Integration Tests**: The test suite provides comprehensive usage examples

Proper documentation is one of the next steps in development

## 📄 License

AGPLv3, see the LICENSE.md file for details.
