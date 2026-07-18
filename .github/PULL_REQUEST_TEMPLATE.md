## Summary

Briefly describe what this PR does and why.

## Type of change

- [ ] Bug fix
- [ ] New feature
- [ ] Refactor / cleanup
- [ ] Documentation
- [ ] CI / infrastructure

## Checklist

- [ ] I read [CONTRIBUTING.md](../CONTRIBUTING.md) and the [Code of Conduct](CODE_OF_CONDUCT.md)
- [ ] `FOVERIN_SKIP_EBPF=1 cargo fmt --all` passes
- [ ] `FOVERIN_SKIP_EBPF=1 cargo clippy -p foverin-common -p foverin --all-targets -- -D warnings` passes (or explain why not)
- [ ] Userspace logic builds with `FOVERIN_SKIP_EBPF=1`
- [ ] Full eBPF build tested locally when touching `foverin-ebpf/` or the daemon loader
- [ ] Docs / `SECURITY.md` updated if behavior or trust boundaries change

## Test plan

How did you verify this?

-
-
