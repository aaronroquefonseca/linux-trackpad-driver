#!/usr/bin/env python3
"""Activation-policy regression tests; run with Fedora's systemd-rpm-macros.

Build disposable payload-only RPM fixtures, never install them or execute their
scriptlets. The macro mutation also queries the real project spec.
"""

import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


HERE = Path(__file__).resolve().parent
CHECKER = HERE / "check-activation.py"
SPEC = HERE / "linux-trackpad-driver.spec"
module = importlib.util.spec_from_file_location("activation", CHECKER)
activation = importlib.util.module_from_spec(module)
module.loader.exec_module(activation)


class ActivationPolicyTests(unittest.TestCase):
    def check_cli(self, option, source, forbidden=None):
        result = subprocess.run([sys.executable, str(CHECKER), option, str(source)],
                                capture_output=True, text=True)
        if forbidden:
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn(forbidden, result.stderr)
        else:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_direct_activation(self):
        # Independent examples cover verbs, intervening flags, command paths,
        # quoting and continuations without executing any shell commands.
        for script in (
            "systemctl enable example.service",
            "systemctl --no-reload --now enable example.service",
            "/usr/bin/systemctl --system start example.service",
            "systemctl --no-reload preset example.service",
            "systemctl --preset-mode=enable-only preset-all",
            "systemctl reenable example.service",
            "systemctl restart example.service",
            "systemctl reload-or-restart example.service",
            '"/usr/bin/systemctl" \\\n                --no-reload preset example.service',
            "/usr/lib/systemd/systemd-update-helper install-system-units example.service",
            "/usr/lib/systemd/systemd-update-helper install-user-units example.service",
            "/usr/lib/systemd/systemd-update-helper mark-restart-system-units example.service",
            "%systemd_post example.service",  # missing macros must not hide activation
        ):
            with self.subTest(script=script), self.assertRaises(ValueError):
                activation.check_scriptlets(script)

    def test_reload_and_removal(self):
        activation.check_scriptlets("""
# Documentation: systemctl enable example.service
echo 'activation is opt-in: systemctl --now enable example.service'
systemctl daemon-reload
udevadm control --reload-rules
if [ "$1" -eq 0 ]; then
    systemctl --no-reload --now disable example.service
    systemctl stop example.service
    /usr/lib/systemd/systemd-update-helper remove-system-units example.service
fi
""")

    def test_real_spec_mutation(self):
        self.check_cli("--spec", SPEC)
        with tempfile.TemporaryDirectory() as directory:
            mutated = Path(directory) / "mutated.spec"
            mutated.write_text(SPEC.read_text().replace(
                "\n%post\n", "\n%post\n%systemd_post %{name}.service\n", 1))
            expanded = subprocess.check_output(["rpmspec", "--parse", str(mutated)], text=True)
            # Ensure this tests Fedora's actual helper expansion, not a missing
            # tool/macro or just recognition of the literal macro name.
            self.assertIn("systemd-update-helper install-system-units", expanded)
            self.assertNotIn("%systemd_post ", expanded)
            self.check_cli("--spec", mutated, "automatic activation: systemd-update-helper install-system-units")
            for section in ("preuntrans", "postuntrans"):
                with self.subTest(section=section):
                    mutated.write_text(SPEC.read_text().replace(
                        "\n%files\n", f"\n%{section}\n"
                        "systemctl --system start %{name}.service\n\n%files\n", 1))
                    self.check_cli("--spec", mutated, "automatic activation: systemctl start")

    def test_built_rpm_scriptlets(self):
        lifecycle = "%post\n" + SPEC.read_text().split("\n%post\n", 1)[1].split("\n%files\n", 1)[0]
        header = """Name: activation-policy-fixture
Version: 1
Release: 1
Summary: Disposable scriptlet policy fixture
License: MIT
BuildArch: noarch
%description
Never installed; only RPM metadata is queried.
%install
mkdir -p %{buildroot}/usr/share/activation-policy-fixture
touch %{buildroot}/usr/share/activation-policy-fixture/payload
"""
        cases = [
            ("post", "", None),
            ("post", "%systemd_post %{name}.service\n", "systemd-update-helper install-system-units"),
            ("post", "systemctl --no-reload preset %{name}.service\n", "systemctl preset"),
        ]
        for section in ("preuntrans", "postuntrans"):
            cases.extend([
                (section, "systemctl --system start %{name}.service\n", "systemctl start"),
                (section, "systemctl --system stop %{name}.service\n"
                 "systemctl --no-reload disable %{name}.service\n", None),
            ])
        for section, mutation, forbidden in cases:
            with self.subTest(section=section, mutation=mutation), tempfile.TemporaryDirectory() as directory:
                top = Path(directory)
                spec = top / "fixture.spec"
                scriptlets = (lifecycle.replace("%post\n", "%post\n" + mutation, 1)
                              if section == "post" else lifecycle + f"\n%{section}\n" + mutation)
                spec.write_text(header + scriptlets
                                + "\n%files\n/usr/share/activation-policy-fixture/payload\n")
                expected = f"automatic activation: {forbidden}" if forbidden else None
                self.check_cli("--spec", spec, expected)
                build = subprocess.run(["rpmbuild", "-bb", "--define", f"_topdir {top}", str(spec)],
                                       capture_output=True, text=True)
                self.assertEqual(build.returncode, 0, build.stdout + build.stderr)
                rpm, = top.glob("RPMS/noarch/*.rpm")
                scripts = subprocess.check_output(["rpm", "-qp", "--scripts", str(rpm)], text=True)
                self.assertIn("remove-system-units", scripts)
                if mutation.startswith("%systemd_post"):
                    self.assertIn("systemd-update-helper install-system-units", scripts)
                if section in ("preuntrans", "postuntrans"):
                    body = subprocess.check_output(
                        ["rpm", "-qp", "--queryformat", f"%{{{section.upper()}}}", str(rpm)], text=True)
                    self.assertEqual(mutation.replace("%{name}", "activation-policy-fixture").strip(), body.strip())
                self.check_cli("--rpm", rpm, expected)


if __name__ == "__main__":
    unittest.main(verbosity=2)
