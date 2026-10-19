#!/usr/bin/env python3
"""Generate docs/KEYS.md from code truth (no hand-typed name->code tables).

Sources (all parsed, never hand-copied):
  - src/action_tokens.rs : KEY_TOKEN_CODES table, letter_digit_code arms,
    fn_code rules (the single token->code source of truth)
  - /usr/include/linux/input-event-codes.h : code -> kernel macro name(s)

Grouping by token name is presentation-only (codes always come from the
source above). Unknown tokens fall into "misc" so a map addition can never
silently drop a row -- the drift pin (keys_md_pins_generated_table) only
checks names/codes.

Usage:
  tools/gen_keys_md.py                 # rewrite docs/KEYS.md
  tools/gen_keys_md.py --check         # exit 1 + diff if docs/KEYS.md drifts
"""

import argparse
import difflib
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TOKENS_RS = ROOT / "src" / "action_tokens.rs"
OUT = ROOT / "docs" / "KEYS.md"
DEFAULT_HEADER = "/usr/include/linux/input-event-codes.h"

# Group assignment is presentation-only. Anything not listed lands in "misc".
GROUPS = [
    ("mouse buttons", {"left", "right", "middle", "back", "forward"}),
    (
        "modifiers",
        {
            "ctrl",
            "control",
            "lctrl",
            "rctrl",
            "shift",
            "lshift",
            "rshift",
            "alt",
            "lalt",
            "ralt",
            "meta",
            "super",
            "cmd",
            "command",
            "win",
            "lmeta",
            "rmeta",
        },
    ),
    (
        "nav/edit",
        {
            "esc",
            "escape",
            "enter",
            "return",
            "space",
            "tab",
            "backspace",
            "delete",
            "insert",
            "print",
            "home",
            "end",
            "pageup",
            "pgup",
            "pagedown",
            "pgdn",
            "up",
            "arrow_up",
            "down",
            "arrow_down",
            "left_arrow",
            "right_arrow",
        },
    ),
    (
        "media",
        {
            "volup",
            "volumeup",
            "voldown",
            "volumedown",
            "mute",
            "micmute",
            "play",
            "playpause",
            "playcd",
            "pausecd",
            "pause",
            "stop",
            "stopcd",
            "next",
            "nextsong",
            "prev",
            "previous",
            "previoussong",
        },
    ),
    (
        "browser",
        {
            "browserback",
            "browser_back",
            "browserforward",
            "browser_forward",
            "refresh",
            "homepage",
            "search",
            "favorites",
            "bookmarks",
            "www",
        },
    ),
    (
        "app",
        {"calc", "mail", "computer", "player", "screenlock", "lock"},
    ),
]


def fail(msg: str) -> "NoReturn":  # type: ignore[name-defined]
    raise SystemExit(f"gen_keys_md: error: {msg}")


def region(text: str, start_marker: str, end_marker: str) -> str:
    start = text.find(start_marker)
    if start < 0:
        fail(f"marker not found: {start_marker!r}")
    end = text.find(end_marker, start + len(start_marker))
    if end < 0:
        fail(f"marker not found: {end_marker!r}")
    return text[start:end]


def parse_token_map(tokens_src: str) -> dict:
    """token -> code, extracted from the shared table + helpers (no hand tables)."""
    mapping: dict[str, int] = {}

    # Static table: ("name", code) rows (canonical first; codes literal).
    body = region(tokens_src, "pub const KEY_TOKEN_CODES", "];")
    for tok, code in re.findall(r'\(\s*"([^"]+)"\s*,\s*(\d+)\s*\)', body):
        if tok in mapping:
            fail(f"duplicate token {tok!r} in KEY_TOKEN_CODES")
        mapping[tok] = int(code)

    # Single-char letters/digits: arms like 'a' => 30 inside letter_digit_code.
    ld_body = region(tokens_src, "fn letter_digit_code", "fn fn_code")
    for char, code in re.findall(r"'([^']+)'\s*=>\s*(\d+),", ld_body):
        if len(char) != 1:
            fail(f"unexpected multi-char arm {char!r} in letter_digit_code")
        if char in mapping:
            fail(f"duplicate token {char!r} from letter_digit_code")
        mapping[char] = int(code)

    # Fn keys: range rule (N..=M => base + n) + single rules (N => code).
    fn_body = region(tokens_src, "fn fn_code", "pub fn is_known_token")
    covered: dict[int, int] = {}
    for lo, hi, base in re.findall(
        r"(\d+)\.\.=(\d+)\s*=>\s*Some\(\s*(\d+)\s*\+\s*u16::from\(n\)\s*\)",
        fn_body,
    ):
        for n in range(int(lo), int(hi) + 1):
            covered[n] = int(base) + n
    for n, code in re.findall(
        r"(?<!\.\.)(?<!\d)(\d+)\s*=>\s*Some\(\s*(\d+)\s*\)\s*,", fn_body
    ):
        # Skip the range-rule line's numbers (it has no trailing comma form).
        covered[int(n)] = int(code)
    if not covered:
        fail("no F-key rules parsed from fn_code")
    for n in sorted(covered):
        tok = f"f{n}"
        if tok in mapping:
            fail(f"duplicate token {tok!r} from fn_code")
        mapping[tok] = covered[n]
    return mapping


def parse_header_macros(header_path: str) -> dict:
    """code -> [macro names], from linux/input-event-codes.h."""
    text = Path(header_path).read_text()
    values: dict[str, int] = {}
    pending_alias: dict[str, str] = {}
    for line in text.splitlines():
        m = re.match(r"#define\s+(\w+)\s+([^/\n]+)", line)
        if not m:
            continue
        name, raw = m.group(1), m.group(2).strip()
        if not (name.startswith("KEY_") or name.startswith("BTN_")):
            continue
        raw = raw.split()[0]
        try:
            values[name] = int(raw, 0)
        except ValueError:
            if re.fullmatch(r"[A-Z_][A-Z0-9_]*", raw):
                pending_alias[name] = raw  # e.g. KEY_SCREENLOCK -> KEY_COFFEE
            # else: expression (e.g. KEY_CNT -> (KEY_MAX+1)); not a real code.
    for name, target in pending_alias.items():
        if target not in values:
            continue  # expression-based; not a real code (e.g. KEY_CNT)
        values[name] = values[target]
    by_code: dict[int, list] = {}
    for name, code in values.items():
        by_code.setdefault(code, []).append(name)
    for names in by_code.values():
        names.sort()
    return by_code


def parse_excluded_doc(tokens_src: str) -> list:
    """Deliberately-excluded bullets, verbatim from the token_code docs."""
    start = tokens_src.find("Deliberately excluded")
    if start < 0:
        fail("Deliberately excluded docs missing from src/action_tokens.rs")
    end = tokens_src.find("pub fn token_code", start)
    lines = []
    for line in tokens_src[start:end].splitlines():
        line = line.strip()
        if line.startswith("///"):
            lines.append(line[3:].lstrip())
    # Drop the header line itself; join wrapped continuation lines onto
    # their `- ...` bullet so the reasons stay verbatim and complete.
    bullets = []
    for ln in lines:
        if ln.startswith("- "):
            bullets.append(ln)
        elif bullets and ln and not ln.startswith("- "):
            bullets[-1] += " " + ln
    if not bullets:
        fail("no exclusion bullets parsed from src/action_tokens.rs docs")
    return bullets


def group_of(token: str) -> str:
    for group, members in GROUPS:
        if token in members:
            return group
    if len(token) == 1:
        return "letters/digits"
    if re.fullmatch(r"f\d+", token):
        return "F1-F24"
    return "misc"


GROUP_ORDER = [name for name, _ in GROUPS] + ["letters/digits", "F1-F24", "misc"]


def render(mapping: dict, macros: dict, excluded: list) -> str:
    distinct_codes = len(set(mapping.values()))
    back_code = mapping["back"]
    fwd_code = mapping["forward"]
    back_macros = ", ".join(f"`{m}`" for m in macros[back_code])
    fwd_macros = ", ".join(f"`{m}`" for m in macros[fwd_code])

    rows = []
    for tok, code in mapping.items():
        if code not in macros:
            fail(f"code {code} for {tok!r} missing from kernel header")
        macro_cell = ", ".join(f"`{m}`" for m in macros[code])
        rows.append((tok, code, macro_cell, group_of(tok)))
    rows.sort(key=lambda r: (GROUP_ORDER.index(r[3]), r[1], r[0]))

    out = []
    out.append("# Clickable keys\n")
    out.append(
        "> Generated by `tools/gen_keys_md.py` from `src/action_tokens.rs`"
        " (the shared token table) and"
        " `/usr/include/linux/input-event-codes.h`. Do not edit by hand;"
        " regeneration or `--check` fails CI on drift."
    )
    out.append("")
    out.append(
        f"Every clickable key name: **{len(rows)} names, {distinct_codes} distinct"
        " codes**. Each row's code is the decimal `EV_KEY` code emitted on the"
        " uinput node, with its kernel macro(s) from"
        " `linux/input-event-codes.h`."
    )
    out.append("")
    out.append("## Combo syntax")
    out.append("")
    out.append(
        "`click_*` / `force_*` accept `+`-joined key names (e.g. `ctrl+c`,"
        " `ctrl+refresh`). Names are case-insensitive, surrounding whitespace is"
        " ignored, and every segment must be a known key from the table below"
        " (`parse_click_action` rejects unknown segments). Combos accept every"
        " known key, not just modifiers+letters."
    )
    out.append("")
    out.append("## Mouse-button note")
    out.append("")
    out.append(
        f"`back` / `forward` are **mouse buttons** ({back_macros} ="
        f" {back_code}, {fwd_macros} = {fwd_code}) -- the long-standing"
        " `click_4` / `click_5` mapping. The browser AC Back/Forward keys"
        " (`KEY_BACK` = 158, `KEY_FORWARD` = 159) live under the separate"
        " `browserback` / `browserforward` names (plus `_`-spelled aliases)."
    )
    out.append("")
    out.append("## Key table")
    out.append("")
    out.append("| name | code (dec) | kernel macro | group |")
    out.append("| --- | --- | --- | --- |")
    for tok, code, macro_cell, group in rows:
        out.append(f"| `{tok}` | {code} | {macro_cell} | {group} |")
    out.append("")
    out.append("## Deliberately excluded")
    out.append("")
    out.append("Mirrored verbatim from the `token_code` docs -- these")
    out.append("tokens stay unmapped on purpose:")
    out.append("")
    for bullet in excluded:
        out.append(f"- {bullet[2:]}")
    out.append("")
    out.append("## Verification")
    out.append("")
    out.append(
        "- `tools/gen_keys_md.py --check` regenerates and diffs (CI pin)."
        " Unit test `keys_md_pins_generated_table` (`src/proxy.rs`) reads this"
        " file and asserts every row matches the live `key_token_to_code` map"
        " plus row-count equality, so table drift fails `cargo test`."
        " Every code above was verified against"
        " `/usr/include/linux/input-event-codes.h` at generation time; the"
        " existing `all_mapped_keycodes_match_kernel_header` test is the"
        " standing oracle."
    )
    out.append("")
    return "\n".join(out)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true")
    ap.add_argument("--out", default=str(OUT))
    ap.add_argument("--header", default=DEFAULT_HEADER)
    args = ap.parse_args()

    tokens_src = TOKENS_RS.read_text()
    mapping = parse_token_map(tokens_src)
    if not mapping:
        fail("no tokens parsed from src/action_tokens.rs")
    macros = parse_header_macros(args.header)
    excluded = parse_excluded_doc(tokens_src)
    text = render(mapping, macros, excluded)

    out_path = Path(args.out)
    if args.check:
        current = out_path.read_text() if out_path.exists() else ""
        if current != text:
            diff = difflib.unified_diff(
                current.splitlines(keepends=True),
                text.splitlines(keepends=True),
                fromfile=str(out_path),
                tofile="generated",
            )
            sys.stdout.writelines(diff)
            print("gen_keys_md: docs/KEYS.md drifts from code truth", file=sys.stderr)
            return 1
        print(f"gen_keys_md: {out_path} in sync ({len(mapping)} names)")
        return 0
    out_path.write_text(text)
    print(f"gen_keys_md: wrote {out_path} ({len(mapping)} names)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
