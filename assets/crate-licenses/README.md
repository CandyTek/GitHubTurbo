# Supplied crate license texts

`scripts/build.ps1` copies each dependency's LICENSE/COPYING/NOTICE files into
`dist/licenses/<name>-<version>`. A few crates on crates.io were published without
those files. Their upstream texts are kept here and used only when the crate itself
has none; the build fails if any dependency still ends up without a license text.

| Folder | License | Source |
| --- | --- | --- |
| `asn1-rs-impl-0.2.0` | MIT OR Apache-2.0 | rusticata/asn1-rs (the crate is its `impl` directory); same files as the `asn1-rs` crate |
| `yasna-0.5.2` | MIT OR Apache-2.0 | qnighy/yasna.rs at commit b7e65f9a4c317494cce2d18ea02b3d6eaaea7985, the commit the 0.5.2 crate was published from |
