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
| base-init | T3 `3ddd0a73` — origin work DONE (`bba35b5` skeleton, `55ed6c9` L0 fix, `1628a22` L2 polish; 98+5 verified) | `main` | Skeleton + core-init + review fixes | DONE, SETTLED |
| research-haptic | DONE, SETTLED | — | Haptic survey | DONE |
| research-kernel | DONE, SETTLED | — | Input-stack survey | DONE |
| research-haptic | T3 `20ea67a8` — report received (10.3 KB, ≥8 sources), deltas merged to `docs/RESEARCH.md` §6 | — | Laptop haptic deep survey; tier-split verdict | DONE, SETTLED |
| research-kernel | T3 `33187ad9` — report received (16.4 KB, 18 sources), deltas merged to `docs/RESEARCH.md` §6 | — | Input-stack deep survey; plan-element verdicts | DONE, SETTLED |
| research-tiers | DONE — subagent proxies merged by coordinator | docs only | Tier correction adopted: L0/L1a/L1b/L2s/L2p → `docs/RESEARCH.md` §4 | DONE |

Research output: single `docs/RESEARCH.md` (PR to `main`). Must not block base-init; must inform Phase 1.

| base-init | T3 `3ddd0a73` — 2 commits pushed (`bba35b5` + `6aa419b`), coordinator verified (build/test 39 green, clippy/fmt clean, no clicks deps) | `main` (direct) | Skeleton + core-init | DONE, MERGED. Retro-review: 3 threads (`f7dfd72c` Spark Xhigh, `8c876b6d` MiMo Xhigh, `ca101ddb` Step5 High) on HEAD `6aa419b` | IN REVIEW |

## Phase 1 — wave 1 (base `6aa419b`, dispatched 2026-10-14, one writer per branch)

| ID | Thread | Branch/worktree | Scope | Status |
|----|--------|-----------------|-------|--------|
| config-schema | T3 `e1271c64` — PR #5 `981b6bf6`, coordinator-verified (55 green) | `feat/config-schema` | Full TOML schema + validation | DONE, MERGED (`723f885`). Owner SETTLED |
| apple-parse | T3 `30782f2f` — PR #2 `3d5a18c` (fixes verified, head==PR head, green) | `feat/apple-parse` | 9B parser, pressure@7 | DONE, MERGED (`8910089`). Owner + 3 reviewers SETTLED |
| udev-systemd | T3 `90edb8da` — PR #1, verified (rules + 5 packaging tests) | `feat/udev-systemd` | rules+unit+INSTALL | DONE, MERGED (`3bf2a23`). Owner SETTLED |
| reconnect-bt | T3 `8a38d26b` — PR #4 | `feat/reconnect-bt` | idempotency, BT, resume re-init | OPEN, REBASING onto fixed main (rebase order sent + verified) |
| tests-hw-mock | T3 `8f5054ba` — PR #3, verified clean-HEAD (52+5 green) | `feat/tests-hw-mock` | mock harness + CI workflow | DONE, MERGED (`25068e5`). Owner SETTLED |

## Phase 1 — wave 2 (blocked on wave-1 merges)

| ID | Branch | Depends | Scope | Status |
|----|--------|---------|-------|--------|
| uinput-proxy | `feat/uinput-proxy` | apple-parse + config-schema merged | evdev grab + uinput emit, 1–5 map | QUEUED |
| suspend-hook | `feat/suspend-hook` | reconnect-bt merged | resume re-init, re-attach | QUEUED |

## Phase 1 — wave 3

| ID | Branch | Depends | Scope | Status |
|----|--------|---------|-------|--------|
| force-click | `feat/force-click` | uinput-proxy merged | 2nd level + force actions | QUEUED |

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
- 2026-10-14: Phase-0 wave dispatched as real T3 threads (supervisor skill).
  base-init `3ddd0a73`, research-haptic `20ea67a8`, research-kernel `33187ad9`.
  All creates + first turns accepted, handoffs byte-verified in projections.
  Model: Muse Spark 1.3 Contributor Xhigh (opencode/instance, build agent).
  Prior subagent research kept as §2–§4 baseline in `docs/RESEARCH.md`;
  T3 research threads deepen it and report in-thread (no commits).
- 2026-10-14: T3 research DONE, deltas merged (§6), threads SETTLED.
  Polling posture 500 Hz → capped ~110; ROADMAP/ARCHITECTURE amended.
- 2026-10-14: base-init DONE+pushed (`bba35b5`, `6aa419b`), coordinator-verified.
  Wave 1 dispatched (5 impl + 3 retro-reviews). PRs #1/#2/#3 OPEN+linked.
  Spark review REQUEST-CHANGES, L0/L1 verified REAL in code, verbatim
  follow-up relayed to base-init (see `docs/REVIEWS/base-init-6aa419b.md`).
  Step5 thread replaced (`c5969de2`, corrected model id) after provider error.
- 2026-10-14: base-init fix DONE+pushed (`55ed6c9`, all 13 review findings),
  coordinator-verified clean-HEAD (79+5 green, clippy/fmt clean). RESEARCH §4
  tier label clarified per ap2-spark finding #2 (MT2 = L2p, L1a-class pressure).
  ap2-spark APPROVE-WITH-NOTES (3×L1, no L0); ap2-mimo + ap2-step5c pending.
  PRs #1/#3/#5 MERGED; #2 in review; #4 held for L0-fix rebase.
- 2026-10-14: ap2 reviews 3/3 APPROVE-WITH-NOTES, no L0 → combined L1 follow-up
  relayed to apple-parse (`ebd238a0…`). Mimo base-init review adds 5 small L2s →
  relayed to base-init (`4f805627…`). Known gaps: reconnect ignores user config
  (later task); suspend-hook + uinput-proxy queued (wave 2).
