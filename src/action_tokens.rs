//! Shared action-token definitions: single source of truth for click,
//! force and slide key/button names.
//!
//! [`token_code`] maps every accepted token (lowercased, trimmed by the
//! caller) to its `EV_KEY` code; [`is_known_token`] gates validation
//! ([`crate::config::parse_click_action`]); [`canonical_token`] backs
//! wire-identity comparison
//! ([`crate::config::ActionKind::is_same_action`]); and
//! [`crate::proxy::key_token_to_code`] wraps [`token_code`] for emission.
//! All four agree by construction: one table, no parallel matches.
//!
//! Alias groups share one row code and list the canonical representative
//! first, so [`canonical_token`] (first row with the code) cannot drift
//! from emission. Tokens with distinct codes (`rctrl` vs `ctrl`,
//! `left` vs `left_arrow`, `pause` vs `pausecd`) intentionally keep
//! distinct rows. `back`/`forward` stay mouse buttons (`BTN_SIDE` /
//! `BTN_EXTRA`, codes 275/276); the browser AC Back/Forward keys live
//! under `browserback` / `browserforward`. `play` stays an alias of
//! `KEY_PLAYPAUSE` (164).

/// Static token table: `(token, EV_KEY code)`. Canonical representative
/// first within each alias group. Mouse-button codes mirror the `BTN_*`
/// consts in [`crate::proxy`] (kernel ABI; pinned by
/// `all_mapped_keycodes_match_kernel_header`). Single letters, digits and
/// `f1`-`f24` are intentionally absent here: [`token_code`] resolves them
/// dynamically (see [`all_known_tokens`] for the full universe).
pub const KEY_TOKEN_CODES: &[(&str, u16)] = &[
    // Mouse buttons.
    ("left", 272),
    ("right", 273),
    ("middle", 274),
    ("back", 275),
    ("forward", 276),
    // Modifiers (canonical first; right-hand variants keep their codes).
    ("ctrl", 29),
    ("control", 29),
    ("lctrl", 29),
    ("rctrl", 97),
    ("shift", 42),
    ("lshift", 42),
    ("rshift", 54),
    ("alt", 56),
    ("lalt", 56),
    ("ralt", 100),
    ("meta", 125),
    ("super", 125),
    ("cmd", 125),
    ("command", 125),
    ("win", 125),
    ("lmeta", 125),
    ("rmeta", 126),
    // Navigation / editing.
    ("esc", 1),
    ("escape", 1),
    ("enter", 28),
    ("return", 28),
    ("space", 57),
    ("tab", 15),
    ("backspace", 14),
    ("delete", 111),
    ("insert", 110),
    ("print", 210),
    ("home", 102),
    ("end", 107),
    ("pageup", 104),
    ("pgup", 104),
    ("pagedown", 109),
    ("pgdn", 109),
    ("up", 103),
    ("arrow_up", 103),
    ("down", 108),
    ("arrow_down", 108),
    ("left_arrow", 105),
    ("right_arrow", 106),
    // Media (each code header-verified; see emission docs for exclusions).
    ("volup", 115),
    ("volumeup", 115),
    ("voldown", 114),
    ("volumedown", 114),
    ("mute", 113),
    ("micmute", 248),
    ("play", 164),
    ("playpause", 164),
    ("playcd", 200),
    ("pausecd", 201),
    ("pause", 119),
    ("stop", 166),
    ("stopcd", 166),
    ("next", 163),
    ("nextsong", 163),
    ("prev", 165),
    ("previous", 165),
    ("previoussong", 165),
    // Browser (`back`/`forward` stay mouse buttons above).
    ("browserback", 158),
    ("browser_back", 158),
    ("browserforward", 159),
    ("browser_forward", 159),
    ("refresh", 173),
    ("homepage", 172),
    ("search", 217),
    ("favorites", 364),
    ("bookmarks", 156),
    ("www", 150),
    // App launchers / system.
    ("calc", 140),
    ("mail", 155),
    ("computer", 157),
    ("player", 387),
    ("screenlock", 152),
    ("lock", 152),
];

/// Map a key/button token (already lowercased + trimmed) to its `EV_KEY`
/// code. Static rows come from [`KEY_TOKEN_CODES`]; single characters
/// (`a`-`z`, `0`-`9`) and `f1`-`f24` resolve dynamically. Returns `None`
/// for unmapped tokens.
///
/// Deliberately excluded (would confuse consumers on a uinput trackpad):
/// - `KEY_FN` (464): the Fn modifier itself — no standalone meaning.
/// - `KEY_FN_ESC` (465), `KEY_FN_F1..F12` (466..477), `KEY_FN_1/2/D/E/F/S/B`
///   (478..484), `KEY_FN_RIGHT_SHIFT` (485): driver-internal Fn-row reports
///   consumed inside keyboard firmware/drivers, not meaningful output keys.
/// - `KEY_PLAY` (207), `KEY_STOP` (128, AC Stop), `KEY_PROPS` (130),
///   `KEY_STOP_RECORD`/`KEY_PAUSE_RECORD` (609/610): obscure duplicates of
///   covered keys (`playpause`/`stopcd`) or record-specific keys no
///   gesture user plausibly wants; emitting them risks confusing mappings.
pub fn token_code(token: &str) -> Option<u16> {
    for (name, code) in KEY_TOKEN_CODES {
        if *name == token {
            return Some(*code);
        }
    }
    if token.len() == 1 {
        return letter_digit_code(token);
    }
    if token.starts_with('f') {
        return fn_code(token);
    }
    None
}

fn letter_digit_code(s: &str) -> Option<u16> {
    let c = s.chars().next()?;
    let code = match c {
        'a' => 30,
        'b' => 48,
        'c' => 46,
        'd' => 32,
        'e' => 18,
        'f' => 33,
        'g' => 34,
        'h' => 35,
        'i' => 23,
        'j' => 36,
        'k' => 37,
        'l' => 38,
        'm' => 50,
        'n' => 49,
        'o' => 24,
        'p' => 25,
        'q' => 16,
        'r' => 19,
        's' => 31,
        't' => 20,
        'u' => 22,
        'v' => 47,
        'w' => 17,
        'x' => 45,
        'y' => 21,
        'z' => 44,
        '1' => 2,
        '2' => 3,
        '3' => 4,
        '4' => 5,
        '5' => 6,
        '6' => 7,
        '7' => 8,
        '8' => 9,
        '9' => 10,
        '0' => 11,
        _ => return None,
    };
    Some(code)
}

/// `KEY_F1..F10` are contiguous (`59..=68`); `F11`/`F12` jump to 87/88
/// (`69`/`70` are `NUMLOCK`/`SCROLLLOCK` — a prior rev mapped them wrong).
/// `KEY_F13..F24` are contiguous (`183..=194`); each value verified
/// individually against `linux/input-event-codes.h`, never extrapolated.
fn fn_code(s: &str) -> Option<u16> {
    let n: u8 = s[1..].parse().ok()?;
    match n {
        1..=10 => Some(58 + u16::from(n)),
        11 => Some(87),
        12 => Some(88),
        13 => Some(183),
        14 => Some(184),
        15 => Some(185),
        16 => Some(186),
        17 => Some(187),
        18 => Some(188),
        19 => Some(189),
        20 => Some(190),
        21 => Some(191),
        22 => Some(192),
        23 => Some(193),
        24 => Some(194),
        _ => None,
    }
}

/// Whether `token` (already lowercased + trimmed) is a valid key/button
/// token. Single source for validation: accepts exactly what
/// [`token_code`] maps.
pub fn is_known_token(token: &str) -> bool {
    token_code(token).is_some()
}

/// Canonical representative for a token: aliases resolving to the same
/// kernel code share one representative so comparison sees wire identity,
/// not spelling (`super+left` == `meta+left`, `ctrl+c` == `control+c`).
/// The representative is the first [`KEY_TOKEN_CODES`] row with the
/// token's code; dynamic tokens (`a`-`z`, `0`-`9`, `f1`-`f24`) and
/// unknown tokens map to themselves.
pub fn canonical_token(token: &str) -> &str {
    match token_code(token) {
        Some(code) => KEY_TOKEN_CODES
            .iter()
            .find(|(_, c)| *c == code)
            .map(|(t, _)| *t)
            .unwrap_or(token),
        None => token,
    }
}

/// Every accepted token: static rows plus single letters/digits and
/// `f1`-`f24`. Test/coverage universe (also the `KEYS.md` universe);
/// not used on hot paths.
pub fn all_known_tokens() -> Vec<String> {
    let mut out: Vec<String> = KEY_TOKEN_CODES.iter().map(|(t, _)| t.to_string()).collect();
    for c in 'a'..='z' {
        out.push(c.to_string());
    }
    for c in '0'..='9' {
        out.push(c.to_string());
    }
    for n in 1..=24 {
        out.push(format!("f{n}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_table_has_no_duplicate_tokens_or_misordered_aliases() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for (token, _) in KEY_TOKEN_CODES {
            assert!(seen.insert(*token), "duplicate static token '{token}'");
        }
        // No static token may collide with the dynamic universe.
        for token in all_known_tokens() {
            let static_hits = KEY_TOKEN_CODES.iter().filter(|(t, _)| *t == token).count();
            assert!(static_hits <= 1, "static/dynamic overlap: '{token}'");
        }
        // Canonical representative is the first row of its code group.
        use std::collections::HashMap;
        let mut first: HashMap<u16, &str> = HashMap::new();
        for (token, code) in KEY_TOKEN_CODES {
            first.entry(*code).or_insert(token);
        }
        for token in all_known_tokens() {
            let Some(code) = token_code(&token) else {
                continue;
            };
            if let Some(want) = first.get(&code) {
                assert_eq!(canonical_token(&token), *want, "{token}");
            }
        }
    }

    #[test]
    fn shared_table_drives_validation_comparison_and_output() {
        // Single-enumeration drift guarantee: every accepted token must
        // validate (config), compare by wire identity (canonical), and
        // emit the shared code (proxy). Same-code tokens merge;
        // different-code tokens never do.
        let tokens = all_known_tokens();
        assert!(tokens.len() > 100, "universe too small: {}", tokens.len());
        let mut kinds = Vec::with_capacity(tokens.len());
        for token in &tokens {
            assert!(
                crate::config::parse_click_action(token).is_ok(),
                "'{token}' must validate"
            );
            let code = token_code(token).unwrap_or_else(|| panic!("'{token}' must map"));
            assert_eq!(
                crate::proxy::key_token_to_code(token),
                Some((crate::proxy::EV_KEY, code)),
                "'{token}' must emit the shared code"
            );
            kinds.push(crate::config::parse_click_action(token).unwrap().kind);
        }
        for (i, a) in tokens.iter().enumerate() {
            for (j, b) in tokens.iter().enumerate() {
                let same_wire = token_code(a) == token_code(b);
                assert_eq!(
                    kinds[i].is_same_action(&kinds[j]),
                    same_wire,
                    "'{a}' vs '{b}' must agree with wire codes"
                );
            }
        }
    }

    #[test]
    fn unknown_tokens_stay_rejected_everywhere() {
        for token in [
            "frobnicate",
            "fn",
            "fn_esc",
            "f0",
            "f25",
            "f99",
            "",
            " ",
            "left_arrowx",
            "ctr",
        ] {
            assert_eq!(token_code(token), None, "{token}");
            assert!(!is_known_token(token), "{token}");
            assert!(crate::config::parse_click_action(token).is_err(), "{token}");
            assert_eq!(crate::proxy::key_token_to_code(token), None, "{token}");
        }
        // Boundaries of the dynamic ranges.
        for token in ["a", "z", "0", "9", "f1", "f12", "f13", "f24"] {
            assert!(is_known_token(token), "{token}");
        }
        for token in ["f0", "f25"] {
            assert!(!is_known_token(token), "{token}");
        }
    }
}
