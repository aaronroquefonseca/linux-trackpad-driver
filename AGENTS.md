# Agent instructions

This is a Rust userspace driver for Apple Magic Trackpad 2 on Linux.

## Working on the project

- Keep changes focused, follow existing patterns, and prefer simple implementations. Read the relevant documentation and surrounding code before changing behavior.
- Run formatting, Clippy, and tests appropriate to the change. Report what was actually verified and any remaining limitations.
- Automated tests must use mocks or isolated fixtures, without access to the host trackpad. Skip `real_uinput_create_e2e_on_hardware` unless hardware testing is explicitly requested.
- Do not change installed services, system configuration, device bindings, or packages unless the user requests it. Never commit secrets or expose them in logs.

## Git identity

The AI agent is a tool used by the author, not an author or co-author. The author may describe the tools and methodology used in PR descriptions or other documentation; the agent may write that explanation on the author's behalf. This does not change Git authorship.

- Use the existing configured Git identity. Never replace it with an agent name, model name, invented email, or guessed identity, including through command-line or environment overrides. If it is missing, ask the user.
- Do not add AI or agent `Co-authored-by` trailers.
- Keep commits focused and use Conventional Commit messages.
- Name new branches `<type>/<short-kebab-case-description>`, using `feat`, `fix`, `refactor`, `docs`, `test`, or `chore` as appropriate (for example, `fix/usb-startup-delay` or `docs/testing-guide`). Describe the change, not the agent, model, or session. Keep the existing branch name when continuing a PR.
- Do not merge PRs, rewrite published history, change branch protections, or create tags or releases unless explicitly requested.

## Implementation workflow

- The normal workflow is task → implementation → validation → PR. Complete the requested work and open or update a focused PR, unless the user requests a draft, discussion, or another workflow. Respect requests to review changes before committing.
- Keep the PR specific to the task. Avoid unrelated fixes, refactors, and speculative abstractions. Update documentation when the behavior or usage changes.
- Describe the problem, resulting behavior, validation performed, and meaningful limitations in the PR. Distinguish software checks from physical hardware testing.
- If the user asks you to review your own implementation, delegate the review to a separate subagent with its own context. Never perform that review yourself in the implementation context. Give the reviewer the task requirements and exact revision to review, and let it assess the changes independently.
- When the PR is ready for review or merge, give the user its URL, a short explanation of what to test, and one concrete, copy/paste-ready terminal block for testing that exact PR revision.
- The test block must fetch/check out and build the PR before stopping the installed service, preserving existing local changes. Then stop `linux-trackpad-driver.service` and run the built binary directly in the foreground with `sudo`, using `ltd run` and the installed configuration (normally `/etc/linux-trackpad-driver/config.toml`). Do not install the test binary over the packaged binary.
- Tell the user to exit with Ctrl+C. The block must automatically start the installed service again after the test binary exits, including on interruption or failure. Use a cleanup trap or equivalent so a failed command cannot skip restoration. Provide these commands for the user; do not execute the hardware test yourself unless requested.

## PR reviews

- Review the actual diff and affected behavior for functionality, correctness, regressions, code quality, readability, and simplicity.
- Check how focused the PR is. Prefer a concrete change addressing one problem; identify unnecessary breadth and unrelated changes. Request refactoring only when it helps the PR's actual scope.
- For each finding, explain the trigger, consequence, location, and smallest reasonable correction. Separate merge blockers from optional improvements; avoid speculative findings without supporting evidence.
- Finish with a clear merge-readiness verdict, what was verified, and any remaining limitations. Before an authorized merge, recheck the current head, required CI, conflicts, and unresolved review findings.

These instructions also apply to delegated agents.
