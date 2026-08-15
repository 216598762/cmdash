# Release packaging

Tagged builds are packaged by `.github/workflows/release.yml` for:

- `x86_64-unknown-linux-gnu`
- `aarch64-apple-darwin`
- `x86_64-pc-windows-msvc`

Each target produces a `cmdash-vX.Y.Z-TARGET.tar.gz` archive and matching `.sha256` checksum. Archives contain the release binary, license, and project documentation, but no runtime-generated state.

Validate a downloaded archive with:

```bash
sha256sum --check cmdash-vX.Y.Z-TARGET.tar.gz.sha256
```

The default release is built without optional protocol/runtime features. CI validates separate `sixel` and `wasm-plugins` release builds; publish those variants separately only after their target-specific capability, permission, startup, and size checks pass. The sixel variant emits retained scene submissions only when sixel capability detection succeeds, while the WASM variant keeps modules import-free and WASI-free.
