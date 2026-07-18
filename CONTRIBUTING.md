# Contributing to Foverin

Thanks for helping improve an eBPF-powered Linux optimizer. Please read the
[Code of Conduct](.github/CODE_OF_CONDUCT.md) and [SECURITY.md](SECURITY.md) first.

## Development setup

See the **Installation** section in [README.md](README.md) (Arch/CachyOS,
`llvm`, `clang`, nightly Rust, `bpf-linker`).

```bash
git clone https://github.com/hocestnonsatis/foverin.git
cd foverin
cargo build --release --bin forge
./target/release/forge
cargo build --release --bin foverin-daemon --bin foverin-cli
```

## Workflow

1. Open an issue (or claim an existing one) before large changes.
2. Work on the default branch unless maintainers ask otherwise.
3. Keep PRs focused — one concern per PR when practical.
4. Fill out the pull request template.

## Checks before you push

Userspace CI skips compiling the BPF object. Match that locally:

```bash
FOVERIN_SKIP_EBPF=1 cargo fmt --all
FOVERIN_SKIP_EBPF=1 cargo clippy -p foverin-common -p foverin --all-targets -- -D warnings
FOVERIN_SKIP_EBPF=1 cargo check -p foverin-common --all-features
FOVERIN_SKIP_EBPF=1 cargo check -p foverin --lib --bin foverin-cli --bin forge
```

If you touch `foverin-ebpf/`, `foverin/build.rs`, or the daemon loader, also run
a **full** local build **without** `FOVERIN_SKIP_EBPF` and smoke-test with
`sudo -E ./target/release/foverin-daemon`.

## Project map

| Path | Purpose |
| --- | --- |
| `foverin-ebpf/` | BPF program |
| `foverin/src/brain.rs` | Candle classifier |
| `foverin/src/actuator.rs` | cpufreq governor writes |
| `foverin/src/bin/foverin-daemon.rs` | Privileged daemon + UDS |
| `foverin/src/bin/foverin-cli.rs` | TUI client |
| `foverin-common/` | `ProcessEvent`, `StateSnapshot` |

## License

By contributing, you agree that your contributions are dual-licensed under MIT
OR Apache-2.0, as described in [LICENSE](LICENSE).
