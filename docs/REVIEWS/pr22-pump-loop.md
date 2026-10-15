# Review — PR #22 pump-loop @899e004 (complex → 3 reviewers, 4 rounds)

- Reviewers: Spark (`d531c473`) REQUEST-CHANGES, MiMo (`70d61199`) REQUEST-CHANGES,
  Step5 (`5c4b9d4e`) REQUEST-CHANGES. uinput ioctls/order/sizes verified exact by all.
- Round 1 (Spark L1s, both verified REAL in code by coordinator): sibling-Detached
  kills grab → attached_node ownership gate; bare-`Apple` name_scan → Trackpad-only
  + MT capability gate. Landed `fffbc24`.
- Round 2 (Spark L2 bundle): multi-child pick, init-blocking doc, abs widen, disconnect
  spin-down, panic visibility, haptic-node routing. Landed in round-3 push.
- Round 3 (MiMo deltas): renumber removals-before-scan + replug disconnect-classify
  + re-attach were DROPPED by owner (reverted); coordinator caught via code check
  and re-relayed as final round. Landed `899e004` (+ nits).
- Round 4 (Step5 deltas): numeric discovery sort, NameScan-untrusted-for-grab, haptic
  strict-send + If2 routing, create-fd hygiene, dead-code removal, batched emits.
  Landed `7ac70cc`.
- Merged (`main` 269+12 green, CI ci+deb green on head). Owner + 3 reviewers SETTLED.
- Owner hardware next: remove `evdev.conf` override (auto-resolution), enable proxy,
  expect attach (not `refusing`/`pump not ready`), test clicks/haptics per TESTING.md.
