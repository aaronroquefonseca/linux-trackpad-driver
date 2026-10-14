# Security Policy

## Supported versions

Only the latest `main` and the most recent signed release tag receive
security fixes. Older tags are unsupported — upgrade first, then report.

| Version | Supported |
|---|---|
| latest `main` / latest signed tag | ✅ |
| older tags | ❌ |

## How to report

- Email the maintainer privately: **aaronroquefonseca@gmail.com**.
- Include: `ltd` version (or commit SHA), device VID:PID, distro,
  kernel (`uname -r`), and debug logs (`RUST_LOG=debug`).
- Do **not** open a public issue for a suspected vulnerability.
- Expect acknowledgment within 7 days. Coordinated disclosure:
  a fix + signed release first, public details after.

Out of scope: vulnerabilities in the Linux kernel HID stack, `libinput`,
systemd/udev, or distro packaging toolchains — report those upstream,
but tell us if `ltd` needs a workaround.

## No security through obscurity (HID note)

This driver talks to USB/Bluetooth HID devices via raw feature reports
(`02 01`, `21 01`, `99 F2 XX`) and `/dev/hidrawN`. The protocol bytes are
documented in `docs/RESEARCH.md` and the source on purpose:

- Hiding report IDs or init sequences provides **no** real protection —
  anyone with the hardware and `usbmon`/`btmon` can capture them.
- Real security here means: minimal privileges (drop root where possible),
  no network surface, no secret storage, validated report lengths before
  parsing, and proxy-failure fallback to passthrough.
- If you find a memory-safety or privilege issue (out-of-bounds parse,
  unsafe block misuse, over-permissive udev rules/service unit), that is
  a genuine report — send it per above.

## Secret hygiene

No credentials, tokens, or private keys belong in this repo (code, docs,
configs, or history). PRs must confirm a clean diff; maintainers run a
`git log -p` secret-pattern scan before releases (see `docs/RELEASING.md`).
