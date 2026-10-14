# Roadmap — linux-trackpad-driver (Rust rewrite)

Repo: https://github.com/aaronroquefonseca/linux-trackpad-driver
Reference (read-only, never copy verbatim): `/home/aaronroque/Projects/AppleTrackpadUserSpaceApp`
Default branch: `main` (this repo uses `main`, not `master`; all docs saying
"merged to master" mean "merged to `main`").

## Context

Full rewrite from scratch in Rust. Same functionality as reference, richer
where bugs / wrong assumptions are found. Architecture may differ if cleaner.

Key hardware facts carried over as requirements (reimplement, don't copy):
- Apple Magic Trackpad 2 (Lightning `05ac:0265` / `004c:0265`,
  USB-C `05ac:0324` / `004c:0324`), USB + Bluetooth.
- USB init via feature reports: multitouch `02 01`, host-click `21 01`,
  polling `99 F2 X`. BT path is single-node (no init dance, straight to use).
- Polling cap: issue mr-cal/Linux-Magic-Trackpad-2-USB-C-Driver#26 —
  requested 90 Hz → ~110 Hz effective cap, formula `153,242,X` → `3850/X`.
  New code must express high-rate intent (default request 500 Hz) while
  clamping to device capability, and document the cap honestly.
- Click proxy (opt-in): hidraw 9-byte block parse, pressure at offset 7,
  evdev grab + uinput emit, sensitivity / margin / haptic / quiet-click,
  force second level, 1–5 finger action map.

## Functional layers

1. **core-init (always on).** USB feature-report init, BT single-node path,
   reconnect-on-udev-add, suspend/resume re-init, systemd unit, udev rules.
2. **apple-clicks (opt-in, `enhanced_clicks = false | true`).** hidraw parse,
   evdev grab + uinput proxy, pressure state machine, haptics, force click,
   1–5 finger mapping. Default OFF — device must work perfectly with init
   alone (plain libinput pointer).
3. **generic (future).** Non-Apple trackpads. Most lack host-mode / haptics.
   Capability tiers, trait now, impls later (see Architecture).

## Capability tiers (to validate against `docs/RESEARCH.md`)

| Tier | Pressure | Haptic | Examples | Behavior |
|------|----------|--------|----------|----------|
| L0 | no | no | basic evdev pads | init + passthrough only, no click synthesis |
| L1 | yes | no | some I2C-HID pads | pressure click synthesis, no haptic out |
| L2 | yes | yes | Apple MT2, modern haptic laptops | full proxy: pressure in, haptic out |

Rule: tiers are capability declarations, not vendor allowlists.
`TouchSource` trait now; `AppleHidraw` first; `EvdevGeneric` stubbed.

## Isolation rule (hard)

- init never depends on clicks.
- clicks never block init.
- Click-proxy failure → log + fall back to passthrough. Init path unaffected.

## Phases

- **Phase 0 (now, parallel).** `base-init` skeleton + core-init on `main`
  (only direct-to-`main` exception) CONCURRENT with research threads.
  No ordering dependency either way.
- **Phase 1 (after base-init merges).** Parallel PR threads, one task per
  agent, each on its own branch + PR, CI-ready:
  `udev-systemd`, `reconnect-bt`, `config-schema`, `apple-parse`,
  `uinput-proxy`, `force-click`, `suspend-hook`, `tests-hw-mock`.
- **Phase 2.** Generic tier impls, packaging (deb/AUR), hardening,
  docs site. Scoped after Phase 1 lands.

## Quality gates (enforced on every PR)

- Small commits, Conventional Commits (`feat|fix|docs|refactor|test|chore(scope): intent only`).
- `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test` green per commit.
- No verbatim copy from reference dir. Reimplement.
- Never merge red CI. Never merge with unresolved L0/L1 review findings.
- Complex PRs (proxy, HID parse, uinput, daemon lifecycle): 3 review threads
  (Muse Spark 1.3 Contributor Xhigh, MiMo-V2.6-Flash Xhigh, Step5 Preview Free High),
  each VERDICT + file:line claims; coordinator verifies each claim in code
  before merge / comeback.
