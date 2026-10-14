# Tasks — coordinator tracking (compaction memory)

Default branch: `main`. Phase 0 `base-init` commits directly to `main`
(only exception). Everything else: new branch + PR, CI-ready, one task per agent.

Review policy: trivial (docs/packaging/small fix) → coordinator reviews directly.
Complex (proxy, HID parse, uinput, daemon lifecycle) → 3 review threads
(Spark 1.3 Xhigh, MiMo-V2.6-Flash Xhigh, Step5 High), VERDICT + file:line claims,
coordinator verifies each claim in code. Merge gates: CI green, no unresolved L0/L1.

## Phase 0 — first wave (CONCURRENT, no ordering dependency)

| ID | Thread | Branch | Scope | Status |
|----|--------|--------|-------|--------|
| base-init | NEW T3 thread | `main` (direct) | Bare Rust skeleton + core-init: workspace/binary, `run`+`reconnect` CLI stubs, USB feature reports (`02 01`, `21 01`, `99 F2 X` + 500→cap clamp), BT single-node path, lock socket, `cargo fmt/clippy/test` green | DISPATCHED |
| research-haptic | NEW T3 thread | docs only | Modern laptop haptic trackpads: vendors, HID usages, pressure/force exposure | DISPATCHED |
| research-kernel | NEW T3 thread | docs only | Linux libinput/kernel hid/uinput/evdev status: what APIs expose pressure/force/haptics | DISPATCHED |
| research-tiers | NEW T3 thread (or folded into above) | docs only | Validate L0/L1/L2 tier model against findings → `docs/RESEARCH.md` | DISPATCHED |

Research output: single `docs/RESEARCH.md` (PR to `main`). Must not block base-init; must inform Phase 1.

## Phase 1 — fan-out (blocked on base-init merged to `main`)

| ID | Branch | Depends | Scope | Status |
|----|--------|---------|-------|--------|
| udev-systemd | `feat/udev-systemd` | base-init | udev rules (VID:PID-gated add), systemd unit, install notes | QUEUED |
| reconnect-bt | `feat/reconnect-bt` | base-init | `reconnect` idempotency, `-f/-p/-d`, BT single-node, daemon-socket check | QUEUED |
| config-schema | `feat/config-schema` | base-init | TOML schema + `enhanced_clicks` gate, defaults, validation errors | QUEUED |
| apple-parse | `feat/apple-parse` | base-init (+RESEARCH advisory) | hidraw 9B block parser, pressure@7, multitouch decode, unit tests | QUEUED |
| uinput-proxy | `feat/uinput-proxy` | apple-parse, config-schema | evdev grab + uinput emit, sensitivity/margin/haptic/quiet, 1–5 map | QUEUED |
| force-click | `feat/force-click` | uinput-proxy | second-level `sensitivity+offset`, force actions/haptic | QUEUED |
| suspend-hook | `feat/suspend-hook` | reconnect-bt | resume re-init, pipeline re-attach, never blocks init | QUEUED |
| tests-hw-mock | `feat/tests-hw-mock` | base-init | mock hidraw/evdev harness, polling-cap test (`3850/X`), CI job | QUEUED |

## Phase 2 — later (not yet queued for dispatch)

Generic `EvdevGeneric` impl, packaging (deb/AUR), hardening, docs site.

## Thread-spawn prompts (copy-paste into NEW T3 Code thread, same project/repo)

### base-init
> In repo linux-trackpad-driver, on branch `main` (create if needed), implement
> bare Rust skeleton + core-init from scratch. Do NOT copy code from
> /home/aaronroque/Projects/AppleTrackpadUserSpaceApp (read-only reference).
> Deliver: cargo workspace or single crate that builds; `run` and `reconnect <dev> [-f] [-p Hz] [-d]`
> CLI; USB HID feature-report init sequence (multitouch, host-click, polling with
> requested-500→device-cap clamp, document ~110 Hz cap); Bluetooth single-node path
> (no init dance); single-instance lock socket; `cargo fmt`, `cargo clippy -- -D warnings`,
> `cargo test` green. Small Conventional-Commit commits directly to `main`.
> No click-proxy code in this task.

### research (×2–3 threads, docs-only, can start immediately, no code checkout needed)
> In repo linux-trackpad-driver, research and write `docs/RESEARCH.md` (your section).
> Thread A: survey modern laptop haptic trackpads (Apple, Lenovo, Dell, Framework,
> etc.) — how pressure/force/haptics are exposed over HID. Thread B: Linux stack status
> (kernel hid-multitouch/hid-apple, libinput, evdev, uinput) — which APIs expose
> pressure/force/haptic output today. Thread C (or coordinator merge): validate the
> L0/L1/L2 capability-tier model against A+B, list corrections. Cite sources with URLs.
> Docs-only PR to `main`. Read-only reference allowed for context; no code copying.

## Log

- 2026-10-14: coordinator boot; repo empty (`main`, no commits); wrote
  ROADMAP/ARCHITECTURE/TASKS; dispatched Phase-0 wave.
