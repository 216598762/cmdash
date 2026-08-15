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

The default release is built without optional protocol/runtime features. Build variants such as `--features sixel` or `--features wasm-plugins` should be published separately only after their target-specific capability and size checks pass.
