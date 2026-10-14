## Summary

<!-- One paragraph: what changes and why. -->

## Scope

- [ ] Docs-only / metadata (no driver behavior change)
- [ ] Driver change (core-init, parser, proxy, reconnect, suspend)
- [ ] Config schema change (default.toml + docs updated)
- [ ] Packaging (debian/, udev, systemd)

## Gates

- [ ] `cargo fmt --all -- --check` clean
- [ ] `cargo clippy --all-targets -- -D warnings` clean
- [ ] `cargo test` green (note count: <!-- e.g. 206 + 11 -->)
- [ ] Conventional Commit message(s), small focused diff

## Checklist

- [ ] No secrets/keys/tokens added (see `SECURITY.md`)
- [ ] No driver behavior change unless Scope says so
- [ ] Docs updated if user-visible behavior changed (`README.md`, `INSTALL.md`, `docs/`)
- [ ] Tested on hardware (VID:PID + connection): <!-- or "not applicable / mock only" -->
