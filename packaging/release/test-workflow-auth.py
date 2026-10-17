#!/usr/bin/env python3
"""test-workflow-auth.py — evaluate the actual workflow authorization
conditions under dispatch/release scenarios.

Parses the real `if:` conditions and checkout `ref:` pins from the three
publishing workflows (not copies), then asserts allow/deny for trusted and
untrusted refs. Fails on any missing guard. No network, no secrets.
"""

import re
import sys
from pathlib import Path

WF = Path(__file__).resolve().parent.parent.parent / ".github" / "workflows"


def parse_workflow(path):
    """Extract job `if`s, step `if`s, checkout `ref`s and secret mappings."""
    jobs = {}
    job = None
    step = None
    pending_if = None  # (owner, indent) awaiting folded continuation
    in_jobs = False
    for raw in Path(path).read_text().splitlines():
        line = raw.rstrip("\n")
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        indent = len(line) - len(line.lstrip(" "))
        if indent == 0:
            in_jobs = stripped == "jobs:"
            job = None
            step = None
            pending_if = None
            continue
        if not in_jobs:
            continue
        if pending_if is not None:
            owner, need = pending_if
            if indent > need:
                owner["if"] += " " + stripped
                continue
            pending_if = None
        m = re.match(r"^  ([A-Za-z0-9_-]+):\s*$", line)
        if m and indent == 2:
            job = m.group(1)
            jobs[job] = {"if": None, "steps": [], "secrets": []}
            step = None
            continue
        if job is None:
            continue
        if indent == 4 and stripped.startswith("if:"):
            expr = stripped[3:].strip()
            if expr in (">-", ">"):
                pending_if = (jobs[job], 4)
                jobs[job]["if"] = ""
            else:
                jobs[job]["if"] = expr
            continue
        if indent == 6 and stripped.startswith("- "):
            step = {"name": stripped[2:].strip()[:80], "if": None, "ref": None}
            jobs[job]["steps"].append(step)
            nm = re.match(r"name:\s*(.+)", step["name"])
            if nm:
                step["name"] = nm.group(1).strip()
            continue
        if step is not None and indent == 8 and stripped.startswith("if:"):
            expr = stripped[3:].strip()
            if expr in (">-", ">"):
                pending_if = (step, 8)
                step["if"] = ""
            else:
                step["if"] = expr
            continue
        if step is not None and indent == 8 and stripped == "with:":
            step["in_with"] = True
            continue
        if indent <= 6 and step is not None:
            step.pop("in_with", None)
        if (
            step is not None
            and step.get("in_with")
            and indent == 10
            and stripped.startswith("ref:")
        ):
            step["ref"] = stripped[4:].strip()
            continue
        if "AUR_SSH_PRIVATE_KEY: ${{ secrets" in stripped:
            jobs[job]["secrets"].append(
                {"step": step["name"] if step else None, "indent": indent}
            )
    return jobs


def unwrap(expr):
    expr = expr.strip()
    m = re.match(r"^\$\{\{\s*(.+?)\s*\}\}$", expr, re.S)
    return m.group(1) if m else expr


TOKEN = re.compile(r"\s*(\(|\)|==|!=|&&|\|\||'(?:[^']*)'|[A-Za-z0-9_.]+)")


def tokenize(s):
    pos, out = 0, []
    while pos < len(s):
        if s[pos].isspace():
            pos += 1
            continue
        m = TOKEN.match(s, pos)
        if not m:
            raise ValueError(f"cannot tokenize near {s[pos : pos + 20]!r}")
        out.append(m.group(1))
        pos = m.end()
    return out


class Eval:
    def __init__(self, tokens, ctx):
        self.toks = tokens
        self.pos = 0
        self.ctx = ctx

    def peek(self):
        return self.toks[self.pos] if self.pos < len(self.toks) else None

    def next(self):
        tok = self.peek()
        self.pos += 1
        return tok

    def parse_or(self):
        val = self.parse_and()
        while self.peek() == "||":
            self.next()
            rhs = self.parse_and()
            val = val or rhs
        return val

    def parse_and(self):
        val = self.parse_cmp()
        while self.peek() == "&&":
            self.next()
            rhs = self.parse_cmp()
            val = val and rhs
        return val

    def parse_cmp(self):
        val = self.parse_primary()
        if self.peek() in ("==", "!="):
            op = self.next()
            rhs = self.parse_primary()
            val = (val == rhs) if op == "==" else (val != rhs)
        return val

    def parse_primary(self):
        tok = self.next()
        if tok == "(":
            val = self.parse_or()
            assert self.next() == ")", "unbalanced paren"
            return val
        if tok in ("true", "True"):
            return True
        if tok in ("false", "False"):
            return False
        if tok.startswith("'") and tok.endswith("'"):
            return tok[1:-1]
        return self.ctx.get(tok)


def allows(expr, ctx):
    if expr is None:
        raise AssertionError("missing `if` condition under test")
    return bool(Eval(tokenize(unwrap(expr)), ctx).parse_or())


def check(desc, cond):
    print(("ok: " if cond else "FAIL: ") + desc)
    if not cond:
        sys.exit(1)


def C(event, ref, publish=None, t_stable=None, t_git=None, tag="", gate=""):
    return {
        "github.event_name": event,
        "github.ref": ref,
        "inputs.publish": publish,
        "vars.ENABLE_AUR_STABLE": t_stable,
        "vars.ENABLE_AUR_GIT": t_git,
        "needs.verify.outputs.tag": tag,
        "steps.gate.outputs.publish": gate,
    }


stable = parse_workflow(WF / "aur-stable.yml")
gitwf = parse_workflow(WF / "aur-git.yml")
rel = parse_workflow(WF / "release-tag.yml")
MAIN = "refs/heads/main"
FEAT = "refs/heads/unmerged-packaging"
TAGREF = "refs/tags/v9.9.9"

# --- stable: unmerged execution tree denied before any checkout/script ---
vif = stable["verify"]["if"]
check("stable verify condition exists", vif is not None)
check(
    "dispatch on main may verify",
    allows(vif, C("workflow_dispatch", MAIN, True, "true")),
)
check("release event may verify", allows(vif, C("release", TAGREF, None, "true")))
check(
    "dispatch on feature branch cannot verify",
    not allows(vif, C("workflow_dispatch", FEAT, True, "true")),
)
check(
    "dispatch on tag ref cannot verify",
    not allows(vif, C("workflow_dispatch", TAGREF, True, "true")),
)

pif = stable["push"]["if"]
check("stable push condition exists", pif is not None)
check(
    "release + toggle + tag may push",
    allows(pif, C("release", TAGREF, None, "true", None, "v9.9.9")),
)
check(
    "main retry + toggle + publish may push",
    allows(pif, C("workflow_dispatch", MAIN, True, "true", None, "v9.9.9")),
)
check(
    "valid artifacts do not authorize an unmerged tree",
    not allows(pif, C("workflow_dispatch", FEAT, True, "true", None, "v9.9.9")),
)
check(
    "main publish=false stays dry despite toggle",
    not allows(pif, C("workflow_dispatch", MAIN, False, "true", None, "v9.9.9")),
)
check(
    "unset toggle blocks release push",
    not allows(pif, C("release", TAGREF, None, None, None, "v9.9.9")),
)
check(
    "empty tag blocks push",
    not allows(pif, C("release", TAGREF, None, "true", None, "")),
)

# --- aur-git: branch helper can neither run nor self-authorize ---
gif = gitwf["snapshot"]["if"]
check("git job condition exists", gif is not None)
check("main push may snapshot", allows(gif, C("push", MAIN, None, "true")))
check(
    "main retry may snapshot", allows(gif, C("workflow_dispatch", MAIN, True, "true"))
)
check(
    "feature dispatch never reaches the helper",
    not allows(gif, C("workflow_dispatch", FEAT, True, "true")),
)
check(
    "tag dispatch never reaches the helper",
    not allows(gif, C("workflow_dispatch", TAGREF, True, "true")),
)
gpush = next(
    s["if"] for s in gitwf["snapshot"]["steps"] if s["name"].startswith("push -git")
)
check("git push step condition exists", gpush is not None)
MAL = dict(C("workflow_dispatch", FEAT, True, "true", "true"), **{})  # base
MAL["steps.gate.outputs.publish"] = "true"  # replaced malicious helper
check("malicious helper cannot self-authorize off-main", not allows(gpush, MAL))
check(
    "main push with gate may publish",
    allows(gpush, C("push", MAIN, None, "true", "true", "", "true")),
)
check(
    "gate true but publish=false stays dry",
    not allows(gpush, C("workflow_dispatch", MAIN, False, "true", "true", "", "true")),
)
check(
    "unset toggle blocks gate-allowed push",
    not allows(gpush, C("push", MAIN, None, None, None, "", "true")),
)


# --- trusted checkout pins (structural: the pin is the protection) ---
def checkout_refs(wf, job):
    return [s["ref"] for s in wf[job]["steps"] if s["ref"] is not None]


for job in ("build", "assemble", "draft-release"):
    refs = checkout_refs(rel, job)
    check(
        f"release {job} checks out the exact tag commit",
        any("needs.preflight.outputs.commit" in r for r in refs),
    )
check(
    "stable verify checks out main, not the event ref",
    "main" in checkout_refs(stable, "verify"),
)
for job in ("render", "push"):
    check(
        f"stable {job} reuses the recorded execution tree",
        any("needs.verify.outputs.exec-sha" in r for r in checkout_refs(stable, job)),
    )
check(
    "git snapshot checks out the exact event revision",
    any("github.sha" in r for r in checkout_refs(gitwf, "snapshot")),
)

# --- secret placement: step scope only, never job scope ---
for name, wf, job in (("stable", stable, "push"), ("git", gitwf, "snapshot")):
    maps = wf[job]["secrets"]
    check(f"{name} key mapped exactly once", len(maps) == 1)
    check(
        f"{name} key mapped at step scope, not job scope",
        maps[0]["indent"] == 10
        and maps[0]["step"] is not None
        and maps[0]["step"].startswith("push"),
    )

print("workflow-auth: all contract checks passed")
