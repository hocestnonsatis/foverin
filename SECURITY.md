# Security Policy

## Privileged by design

**`foverin-daemon` must run as root** (or with equivalent capabilities). It:

- Loads and attaches an eBPF program (`sched_process_exec`)
- Writes CPU frequency governors under `/sys/devices/system/cpu/*/cpufreq/`
- Creates and manages `/sys/fs/cgroup/foverin_background/` (cgroup v2)

Do **not** expose the daemon to untrusted users as a “convenience service” without understanding the trust boundary below.

`foverin-cli` is intentionally unprivileged and only consumes telemetry.

## Trust boundary & UDS

| Surface | Mode | Notes |
| --- | --- | --- |
| `/tmp/foverin.sock` | `0666` | Any local user can connect and **read** live `StateSnapshot` JSON (process names, workload labels, governor, cgroup status). This is intentional so a normal desktop session can open the TUI without sudo. |
| Socket contents | NDJSON | Telemetry only today — no command channel. Treat future write APIs as high risk. |
| eBPF / sysfs / cgroup | root | Full machine control for frequency and background isolation. |

### Implications

- **Confidentiality:** Process basenames and optimization decisions are visible to every local account that can open the socket.
- **Integrity:** Clients cannot currently instruct the daemon over UDS. If that changes, authenticate and authorize (e.g. peer credential checks, tighter socket mode, or a dedicated group).
- **Availability:** A local user could open many connections; the daemon should remain robust, but this is not a multi-tenant hardened RPC service.

Hardening ideas if you need a locked-down install: change the socket path to a root-only directory, use mode `0660` + a dedicated `foverin` group, or gate the CLI behind polkit.

## Supported versions

Security fixes are applied on a best-effort basis to the latest tagged release on the default branch.

## Reporting a vulnerability

Please **do not** open a public GitHub issue for exploitable flaws.

1. Email the maintainers listed in the repository profile / `Cargo.toml` authors, **or**
2. Use GitHub’s private vulnerability reporting (Security tab) if enabled.

Include: Foverin version/tag, kernel version, distro, reproduction steps, and impact assessment.

We aim to acknowledge reports within 7 days and coordinate disclosure after a fix or mitigation is available.

## Out of scope (examples)

- “Daemon needs root” — by design for eBPF and sysfs actuation
- Local users reading `/tmp/foverin.sock` telemetry under mode `0666` — documented trade-off
- Misconfiguration of systemd unit paths or missing weight files
- Running untrusted third-party builds of the daemon as root
