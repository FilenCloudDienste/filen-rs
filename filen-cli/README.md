# Filen CLI

📩 [Releases](https://github.com/FilenCloudDienste/filen-cli-releases/releases) | 📖 [Documentation](https://docs.filen.io/docs/cli-rs/readme) | 📜 [Source](https://github.com/FilenCloudDienste/filen-rs/tree/main/filen-cli)

This is a Rust rewrite of [FilenCloudDienste/filen-cli](https://github.com/FilenCloudDienste/filen-cli).

> [!WARNING]
> This project is in active development and still missing most functionality. **Don't use it.**

This rewrite aims to improve the Filen CLI in multiple ways:
- Significantly reduced download size from up to 130MB to ~5MB.
- Significantly improved performance, especially startup performance, which is critical for CLI applications (because we're using Rust instead of TypeScript).
