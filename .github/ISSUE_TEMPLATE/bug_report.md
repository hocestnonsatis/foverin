---
name: Bug report
about: Report a defect in Foverin (daemon, CLI, eBPF, actuator, or docs)
title: "[bug] "
labels: ["bug"]
assignees: []
---

## Summary

A clear, one-sentence description of the bug.

## Environment

- Foverin version / git commit:
- Distro (e.g. CachyOS / Arch):
- Kernel (`uname -r`):
- Rust toolchain (`rustc +nightly -V` if building from source):
- How installed: source / release tarball / systemd

## Steps to reproduce

1.
2.
3.

## Expected behavior

## Actual behavior

## Logs / evidence

```text
# journalctl -u foverin -n 50
# or RUST_LOG=debug sudo -E ./target/release/foverin-daemon
```

Relevant sysfs output if actuator-related:

```text
cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor
cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_available_governors
```

## Additional context

Screenshots, TUI observations, or related PRs.
