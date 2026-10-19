## [0.1.1] - 2026-10-19

### Highlights

- Connect USB trackpads without waiting for the next 30-second discovery scan; device events wake attachment with bounded readiness retries ([#68](https://github.com/aaronroquefonseca/linux-trackpad-driver/pull/68)).
- Recover missing or dead enhanced sessions without restarting the daemon, while keeping failed USB setup ungrabbed ([#67](https://github.com/aaronroquefonseca/linux-trackpad-driver/pull/67)).
- Preserve native Bluetooth input until the proxy successfully grabs it; automatic driver unbinding is removed ([#66](https://github.com/aaronroquefonseca/linux-trackpad-driver/pull/66)).
- Return to native input when pointer output fails, and complete debounced mouse/key releases even when device reports stop ([#65](https://github.com/aaronroquefonseca/linux-trackpad-driver/pull/65), [#64](https://github.com/aaronroquefonseca/linux-trackpad-driver/pull/64)).
- Identify the running build through `ltd --version`, doctor output, and startup logs, including source commit and build channel.
- Diagnose installed configuration and listener state with `ltd doctor` without requiring sudo.
- Add Fedora RPM release packages and repair RPM builds; install Debian configuration as a regular file.

### Details

#### Features

- Embed source identity (build) ([c725071](https://github.com/aaronroquefonseca/linux-trackpad-driver/commit/c72507107318112b54c67ac80742fc0bac802b9e))
- Add version flags (cli) ([299fd35](https://github.com/aaronroquefonseca/linux-trackpad-driver/commit/299fd351e70c632304792398e4b7d6848a785686))
- Build release RPM, document install ([#58](https://github.com/aaronroquefonseca/linux-trackpad-driver/pull/58)) (fedora) ([e5053cb](https://github.com/aaronroquefonseca/linux-trackpad-driver/commit/e5053cba322ac6a798914962ddbbe77fe93a3618))

#### Bug Fixes

- Wake on trackpad hotplug events ([#68](https://github.com/aaronroquefonseca/linux-trackpad-driver/pull/68)) (daemon) ([3857049](https://github.com/aaronroquefonseca/linux-trackpad-driver/commit/3857049f4d5468cf7ac65bfc409c004e9910f17b))
- Retry missing and dead enhanced sessions ([#67](https://github.com/aaronroquefonseca/linux-trackpad-driver/pull/67)) (daemon) ([4006858](https://github.com/aaronroquefonseca/linux-trackpad-driver/commit/4006858986f1170d98224ddc8f1e721bf9f96b01))
- Preserve native input until proxy grabs ([#66](https://github.com/aaronroquefonseca/linux-trackpad-driver/pull/66)) (bluetooth) ([0e824aa](https://github.com/aaronroquefonseca/linux-trackpad-driver/commit/0e824aa8ced989c9035b59ea65f01aaae28feeba))
- Release grabs when output fails ([#65](https://github.com/aaronroquefonseca/linux-trackpad-driver/pull/65)) (uinput) ([506b4c4](https://github.com/aaronroquefonseca/linux-trackpad-driver/commit/506b4c44cbbac062377428a869f4d4bfe470c29c))
- Complete debounced releases during idle ([#64](https://github.com/aaronroquefonseca/linux-trackpad-driver/pull/64)) (clicks) ([a0b1b92](https://github.com/aaronroquefonseca/linux-trackpad-driver/commit/a0b1b923ed034eee8d578239141a815524d0068f))
- Diagnose installed config and listeners without sudo (doctor) ([13ab1c9](https://github.com/aaronroquefonseca/linux-trackpad-driver/commit/13ab1c907196a6541cf512c96663244234639e56))
- Repair RPM packaging and validate builds in CI (fedora) ([18d6666](https://github.com/aaronroquefonseca/linux-trackpad-driver/commit/18d66664196334e80cb318b23098b412c8c29d8f))
- Install configuration as a regular file (debian) ([f30c3b5](https://github.com/aaronroquefonseca/linux-trackpad-driver/commit/f30c3b5048b780e69b5689f6c2ba09e50bde5ecc))

#### Documentation

- Hint at .pacsave on variant switch ([#56](https://github.com/aaronroquefonseca/linux-trackpad-driver/pull/56)) (packaging) ([1c5f284](https://github.com/aaronroquefonseca/linux-trackpad-driver/commit/1c5f284ed6ecf1f2f66c3d5809f75f39d0d22d3a))
- Document AUR and Debian installation (readme) ([7a9f26a](https://github.com/aaronroquefonseca/linux-trackpad-driver/commit/7a9f26a37353fd0635949b66a3e12463b0ecd92a))

#### Miscellaneous

- Enforce release note hygiene (changelog) ([2ef0451](https://github.com/aaronroquefonseca/linux-trackpad-driver/commit/2ef0451b93e569ac7b0d3a3630990d3cb46ed922))
- Refresh description, pkgrel 2 ([#55](https://github.com/aaronroquefonseca/linux-trackpad-driver/pull/55)) (aur) ([204111d](https://github.com/aaronroquefonseca/linux-trackpad-driver/commit/204111d63e7eb21f2022fddaa2ec70d23e988fda))

Full diff: https://github.com/aaronroquefonseca/linux-trackpad-driver/compare/v0.1.0...v0.1.1

## [0.1.0] - 2026-10-17

### Bug Fixes

- Rearm cancelled pending presses (proxy)

- Stagger mixed slide buttons (proxy)

- Advertise and emit ABS_X-Y on the pointer node (proxy)

- Force-hold hysteresis across pressure dips (clicks)

- Pin extras SYN framing, trace dropped cut presses (proxy)

- Stagger cut fresh-press modifiers-first with SYN split (proxy)

- Defer cut re-press past 5ms gap floor (proxy)

- Cut force edge breaks wire under physical hold (proxy)

- Slide level never fires haptic (proxy)

- Stage slide-handoff absorption test with force staging (proxy)

- Force-held release ignores 30ms debounce, old-exact (clicks)

- Stage force edge a frame behind first, old-exact feel (clicks)

- Reselect merge, band reselect, set-compare handoff (review)

- Slide physical hold, cut/handoff gate, Cow slots (review)

- Cut skips empty, handoff adopts pulse, force-first merge (review)

- Send haptic pulses from a FIFO worker, never the pump thread (proxy)

- Evict lifted finger bits so partial lift releases the latch (clicks)

- Correct BTN_TOOL_* codes to kernel header values (proxy)

- Suppress physical BTN_LEFT chord in multi-finger clicks (proxy)

- Emit old-driver normalized touch values (60/800/800) (proxy)

- Convert UP-pulse tests to dual-sink harness (tests)

- Split virtual uinput into pointer + extras nodes (proxy)

- Emit old-service release up-pulse on release edges (haptic)

- Adopt old-driver feel semantics, document parity (clicks,proxy,config)

- Restore cursor motion with touchpad classification set (proxy)

- Resolve If2 sibling numerically, cover owner 4-iface layout (haptic)

- Prefer USB interface 1 for pump node, MT rank tiebreak only (discovery)

- Log selection reason and warn on silent-chosen attach (daemon)

- Rank hidraw selection by MT abs proof, not position (discovery)

- Pass UI_SET_*BIT as int value, not pointer (uinput)

- Same-name Ready re-attaches dead sessions (pump)

- Disconnect classification with session self-release (pump)

- Emit removals before scan loop, renumber heals (tick)

- Batched emits, fd hygiene, drop dead backends (uinput)

- Strict send with fallback, If2 actuator routing (haptic)

- NameScan resolutions never back a grab (pump)

- Numeric sort, input-having interfaces first (discovery)

- Name scan requires Trackpad name plus MT abs bits (evdev)

- Ownership-gated detach, sibling Detached ignored (proxy)

- Edge-trigger BTN_LEFT emission (proxy)

- Hook journal trace with rc and recovery command (resume)

- Self-healing suspend paths with rescan, diff, actionable errors (resume)

- Sync AUR pkgver with HEAD (r74.g4481b60) (packaging)

- Skip build artifacts in manifest check (deb)

- Review-round L2 bundle (force-click)

- Reference-hold keycodes shared across levels (force-click)

- BT presence check, Detached skew, slow re-apply, packaging honesty (resume)

- Cooldown for unreadable verify, single GET_FEATURE per pass (resume)

- One-shot re-arms HW only; clicks re-attach is tick-mediated (resume)

- Hook and daemon honor system config via LTD_CONFIG (resume)

- Review round — button/slot/gate/keycode/detach correctness (proxy)

- Driver-resolved unbind, rebind restore, retry flatten (init)

- Canonicalize device symlink for iface walk (discovery)

- Lock handoff race, help exit 0, retry loop cleanup, doc numbers (review2)

- Review notes — 0xf7 citation, pressure clamp duty, orientation negation (apple-parse)

- Ioctl dir 3, polling floor, fail-closed reconnect, transient-only retry (review)


### Continuous Integration

- Automate reviewed releases and AUR publication (#51) (release)

- Validate debian packaging in CI (deb)


### Documentation

- Prepare repository documentation for public release (#48)

- Update tier and schema section to v1 (arch)

- Rewrite CONFIG.md with Requires column (config)

- Rewrite default.toml to v1 target (config)

- Clarify cut release behavior (config)

- Explain binding timing and flat examples (config)

- Cut-physical merged, all HW-verified (coordinator)

- Slide is valid-but-silent, buttons own haptics (config)

- Force-feel merged (coordinator)

- Gesture-power merged (coordinator)

- Cover slide/spoof/transition keys (config)

- Add CONFIG.md reference from code truth (config)

- Drag+multifinger merged, all HW-verified (coordinator)

- Einval+iface merged (coordinator)

- Pump-loop merged (coordinator)

- Keys table merged (coordinator)

- Generate KEYS.md from code truth with drift pins (key-table)

- Key-coverage merged (coordinator)

- Lo status clarified, key-coverage dispatched (coordinator)

- Owner-bug wave done (coordinator)

- LO minus capture protocol, bug stays open (testing)

- Owner-bug wave dispatched (coordinator)

- Final — 16/16 merged (coordinator)

- Add RELEASING.md enable checklist (release)

- Add pre-public repo-settings checklist (repo)

- Add signed-tag release SOP (release)

- Add SECURITY.md with report policy (security)

- Add bug/feature issue templates and PR template (github)

- Add MIT and Apache-2.0 texts, README license note (license)

- Add distro quickstart, doctor checklist, troubleshooting matrix

- Owner real-trackpad session checklist (testing)

- All merged, deb settled (coordinator)

- Pr8 merged, all phase-1 settled except deb (coordinator)

- Force coexistence spec bullets (coordinator)

- Pr7/pr9/pr10/pr11 merged, status (coordinator)

- INSTALL suspend section for hook, unit, and daemon tick (resume)

- Zombie ledger, status (coordinator)

- Pr4/pr6 merged, wave-2/3 running (coordinator)

- Rb verdicts relayed (coordinator)

- Readme, pr6 reviews (coordinator)

- Add README (status, install, usage, tiers)

- Rb reviews, uinput-proxy dispatched (coordinator)

- Base-init settled, reconnect-bt rebasing (coordinator)

- Pr2 merged, wave-1 settlements (coordinator)

- Ap2 verdicts, arch pressure contract (coordinator)

- Fix verified, tier label clarified (coordinator)

- Wave-1 merges 1-3-5, ap2 reviews (coordinator)

- Wave-1 status, base-init review findings (coordinator)

- Phase-1 wave-1 dispatched (coordinator)

- Record Phase-0 single-crate decision and gaps (architecture)

- T3 research deltas, polling-posture correction (coordinator)

- Phase-0 T3 threads dispatched (coordinator)

- Research findings and tier correction (coordinator)

- Roadmap, architecture, task queue (coordinator)


### Features

- Prepare source, binary, and VCS packages (#50) (aur)

- Sectioned v1 schema with strict validation (config) [!BREAKING]

- Default transition_mode to cut (config) [!BREAKING]

- Use sparse spoof tables under [spoof] (config)

- Honor per-binding combo deadlines (proxy)

- Accept per-binding combo timing (config)

- Human-scale stagger for cut fresh-press via combo_stagger_ms (proxy)

- Slide keys, cut transition, spoof wire (proxy)

- Slide level with press handoff plus spoof pattern (clicks)

- Slide/spoof/transition schema with passthrough defaults (config)

- Real pump wiring with create-before-grab teardown (daemon)

- Real 0x53 actuator report, silent on 0 (haptic)

- Hidraw-to-eventN auto-resolution with fallback chain (evdev)

- Real UI_DEV_SETUP/CREATE, sink-ready grab gate (uinput)

- Hidraw nonblocking source via apple_parse (pump)

- Complete clickable key set (proxy)

- Map F13-F24 key actions with verified kernel codes (clicks)

- Add git-cliff config (changelog)

- Stable PKGBUILD plus nightly template (aur)

- Add inert nightly AUR workflow (release)

- Add inert tag-release workflow (release)

- Add read-only ltd doctor checklist (scan+config+socket) (cli)

- Add postinst, prerm, conffiles, README.Debian (deb)

- Add rules and install manifest (deb)

- Add control, changelog, copyright, source format (deb)

- Second-level arming, hysteresis and proxy coexistence (force-click)

- VCS PKGBUILD with Debian file-set parity (aur)

- Generic Tier-0/1 stub with mock backend (evdev)

- System-sleep hook and resume unit with packaging tests (resume)

- Ltd resume CLI entry for sleep hook and manual re-init (resume)

- Daemon resume scan with verify-or-reinit and hub re-attach (resume)

- Verified resume_usb entry with multitouch-first retry (resume)

- Evdev-grab uinput proxy with haptics and speed scaling (proxy)

- Pressure state machine with hysteresis and finger-count mapping (clicks)

- Env config, disengage restore, usb-only validation (reconnect)

- Harden reconnect path per RESEARCH section 6 (reconnect)

- Verified USB init, BT coexistence policy, resume re-init (init)

- Per-interface routing groundwork, node+bind wait (discovery)

- MT2 9-byte block parser, pressure@7, tier contract (apple-parse)

- Full TOML schema with validation and force fallback (config)

- Hidraw add rules, systemd units, install notes (udev-systemd)

- USB core-init with capped polling, BT path, run/reconnect CLI (init)


### Miscellaneous

- Fix clippy needless_update and cargo fmt (gates)

- Fill crates.io-ready package metadata (manifest)

- Audit .gitignore for rust, debian, editor artifacts (git)

- Add fmt/clippy/test workflow (ci)


### Refactoring

- Rename spoof select to actions_follow_spoof (config)


### Tests

- Retune hysteresis tests to owner tune 30-130-5

- Pin super pairing and tap-hiding across cut cycle under hold (proxy)

- Revive dead force path in shared_left, pin staged UP order (proxy)

- Add config docs drift test (config)

- Pin 2s press+move contract and actuator-stall budget (proxy)

- Scripted hidraw bytes to exact wire assertions (pump)

- Exact-wire purity net for click emissions (proxy)

- Proxy wiring for force level (force-click)

- Serialize env-touching tests via shared lock

- Add shared env lock with concurrent regression test

- Mock hidraw/evdev harness and polling-cap tests (hw-mock)


