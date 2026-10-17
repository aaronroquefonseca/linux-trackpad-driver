# Security Policy

## Supported versions

Pre-release: the latest `main` is the supported version.

Once stable releases are published: the latest stable release is
supported. Older versions get no guaranteed fixes, and there is no LTS.

Reports against older versions are still welcome — include the version
(or commit SHA) so the report can be triaged.

| Version | Supported |
|---|---|
| latest `main` / latest stable release (once published) | ✅ |
| older versions | ❌ no guaranteed fixes |

## How to report

- Email the maintainer privately: **aaronroquefonseca@gmail.com**.
- Include: `ltd` version (or commit SHA), device VID:PID, distro,
  kernel (`uname -r`), and debug logs (see below).
- Do **not** open a public issue for a suspected vulnerability.
- Expect acknowledgment within 7 days. Disclosure timing is coordinated
  with the reporter; this is not a promise of a fix by any specific date.

Out of scope: vulnerabilities in the Linux kernel HID stack, `libinput`,
systemd/udev, or distro packaging toolchains — report those upstream,
but tell us if `ltd` needs a workaround.

## Debug logs for reports

The driver reads its log level from the config file
(`config::init_logging` in `src/config.rs`); `RUST_LOG` is ignored.

1. Set in `/etc/linux-trackpad-driver/config.toml`:
   `log_level = "debug"`
2. `sudo systemctl restart linux-trackpad-driver`
3. `journalctl -u linux-trackpad-driver.service`

Redact sensitive info before sending, and send logs via the private
report email above — never post sensitive diagnostics publicly.

## Failure fallback

If the USB proxy path fails, it releases the evdev grab and the device
falls back to kernel passthrough (`src/proxy.rs`).

On Bluetooth with `enhanced_clicks = true`, the driver may unbind the
HID driver for our VID:PID (`attempt_driver_unbind` in `src/init.rs`).
That unbind is sticky until re-bind/reconnect — there is no guaranteed
automatic recovery, and a failed enhanced path may need re-bind/replug
to restore input. The Bluetooth path is not yet hardware-verified with
this rewrite (issue #42).

## Privileges and attack surface

The shipped systemd unit has no `User=` directive. The shipped service
currently runs as root to access HID and input devices. There is no
privilege dropping today. Sandboxing or privilege reduction are future
ideas only, not current protections.

What is in place today:

- No network listener or remote API. A local Unix socket is used for single-instance locking and daemon liveness detection.
- No secret storage.
- HID report lengths are validated before parsing (`src/apple_parse.rs`).
- The USB proxy never swallows input it cannot re-emit: grab failure,
  pump loss, or detach returns the device to passthrough.

If you find a memory-safety or privilege issue (out-of-bounds parse,
unsafe block misuse, over-permissive udev rules/service unit), that is
a genuine report — send it per above.

## No security through obscurity (HID note)

This driver talks to USB/Bluetooth HID devices via raw feature reports
(`02 01`, `21 01`, `99 F2 XX`) and `/dev/hidrawN`. The protocol bytes are
documented in `docs/RESEARCH.md` and the source on purpose:

- Hiding report IDs or init sequences provides **no** real protection —
  anyone with the hardware and `usbmon`/`btmon` can capture them.
- Real security here means the items in the previous section.

## Secret hygiene

No credentials, tokens, or private keys belong in this repo (code, docs,
configs, or history). PRs must confirm a clean diff; maintainers run a
`git log -p` secret-pattern scan before releases (see `docs/RELEASING.md`).
