# Development Commands

## Build
```bash
cargo check                    # Quick type check
cargo build --release          # Release build
LIBRARY_PATH=/opt/homebrew/lib cargo build --release  # macOS with libkrun
```

## macOS: Post-build signing (required for HVF)
```bash
codesign --entitlements /tmp/bcvk-entitlements.plist -fs - target/release/bcvk
```

## Test
```bash
cargo test                     # Unit tests
bcvk ephemeral run-ssh --execute "echo OK" quay.io/fedora/fedora-bootc:42
```

## Lint
```bash
cargo clippy
cargo fmt --check
```

## Git
```bash
git checkout wip/macos-vfkit   # macOS development branch
git log --oneline main..HEAD   # Changes from main
```
