# Review — base-init @6aa419b (retro, direct-to-main origin)

- PRs linked: #1 udev-systemd, #2 apple-parse, #3 tests-hw-mock.
- Reviewers: Spark Xhigh (`f7dfd72c`) DONE, MiMo Xhigh (`8c876b6d`) running,
  Step5 (`ca101ddb`) ERROR provider model id → replaced by `c5969de2` (corrected id).
- Spark VERDICT: REQUEST-CHANGES. Coordinator verified EVERY L0/L1 claim in code:
  - L0 ioctl dir bits: REAL (`hidraw.h:40-41` dir=3; `init.rs:114-119,163-168` use 1/2 → ENOTTY on HW).
  - L1 polling floor / lock stale-cleanup / hub Mutex+expect / BT disengage event /
    transport fallback: all REAL (file:line confirmed above).
  - L2s (signal latency, BT wait guard, retry set, -p clamp warn, env test, coverage,
    theoretical_hz): plausible, bundled.
- Action: verbatim follow-up relayed to owner `3ddd0a73` (message `70a9071c…`,
  sha256 `c1ab9410…`, verified persisted). Awaiting new HEAD on `main`.
- Merge impact: wave-1 branches do NOT touch `init.rs` ioctl code except
  `reconnect-bt` (daemon/init paths) — it rebases after fix lands. PRs #1/#2/#3
  unaffected (mock harness fakes transport; rules/unit/packaging separate).
