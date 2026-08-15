# Release packaging

Tagged builds are packaged by `.github/workflows/release.yml` as:

- `cmdash-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz`
- a matching `.sha256` checksum file

The archive contains the release binary, license, project documentation, and no runtime-generated state. Validate a downloaded archive with:

```bash
sha256sum --check cmdash-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz.sha256
```

The current package targets Linux x86_64. Additional targets should be added only after backend, PTY, and graphics capability tests run on those platforms.
