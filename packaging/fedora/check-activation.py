#!/usr/bin/env python3
"""Read (never execute) expanded RPM scriptlets and enforce opt-in activation.

This is a conservative command-token check for our simple shell scriptlets,
not a shell interpreter. Keep scriptlets explicit: indirect commands assembled
from variables or eval require review. Removal-time stop/disable is permitted.
"""

import argparse
from pathlib import Path
import shlex
import subprocess
import sys


SCRIPT_TAGS = ("PREIN", "POSTIN", "PREUN", "POSTUN", "PRETRANS", "POSTTRANS")
QUERY_FORMAT = "\n".join(f"%{{{tag}}}" for tag in SCRIPT_TAGS) + "\n"
QUERY_FORMAT += "[%{TRIGGERSCRIPTS}\n][%{FILETRIGGERSCRIPTS}\n][%{TRANSFILETRIGGERSCRIPTS}\n]"
ACTIVATION = {
    "enable", "reenable", "start", "restart", "try-restart",
    "reload-or-restart", "reload-or-try-restart", "isolate", "preset", "preset-all",
}
HELPER_ACTIVATION = {
    "install-system-units", "install-user-units",
    "mark-restart-system-units", "mark-restart-user-units",
}


def check_scriptlets(scriptlets):
    # Preserve command boundaries while allowing shell continuation lines.
    lexer = shlex.shlex(scriptlets.replace("\\\n", ""), posix=True,
                        punctuation_chars=";&|()\n")
    lexer.whitespace = " \t\r"
    lexer.whitespace_split = True
    command = []
    for token in [*lexer, "\n"]:
        if token and set(token) <= set(";&|()\n"):
            for index, word in enumerate(command):
                program = Path(word).name
                forbidden = (ACTIVATION if program == "systemctl" else
                             HELPER_ACTIVATION if program == "systemd-update-helper" else set())
                # Scan arguments rather than assuming the verb immediately
                # follows the executable (e.g. systemctl --no-reload preset).
                for argument in command[index + 1:]:
                    if argument in forbidden:
                        raise ValueError(f"automatic activation: {program} {argument}")
            command = []
        else:
            # Outside Fedora, optional systemd macros may be unavailable.
            # Never silently accept an unexpanded activation macro there.
            if token.startswith("%systemd_") and token not in {
                "%systemd_preun", "%systemd_postun",
                "%systemd_user_preun", "%systemd_user_postun",
            }:
                raise ValueError(f"unexpanded systemd macro: {token}")
            command.append(token)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--rpm", help="built binary RPM to inspect")
    source.add_argument("--spec", help="spec to query with installed RPM macros")
    args = parser.parse_args()
    if args.rpm:
        command = ["rpm", "-qp", "--queryformat", QUERY_FORMAT, args.rpm]
    else:
        command = ["rpmspec", "-q", "--queryformat", QUERY_FORMAT,
                   "-D", "_unitdir /usr/lib/systemd/system",
                   "-D", "_udevrulesdir /usr/lib/udev/rules.d",
                   "-D", "_licensedir /usr/share/licenses", args.spec]
    try:
        scriptlets = subprocess.check_output(command, text=True)
        check_scriptlets(scriptlets)
    except (OSError, subprocess.CalledProcessError, ValueError) as error:
        print(f"check-activation: error: {error}", file=sys.stderr)
        return 1
    print("check-activation: OK (expanded scriptlets; activation remains opt-in)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
