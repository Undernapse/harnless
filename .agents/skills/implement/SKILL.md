---
name: implement
description: "Implement a piece of work based on a spec or set of tickets."
disable-model-invocation: true
---

Implement the work described by the user in the spec or tickets.

Use /tdd where possible, at pre-agreed seams.

Run typechecking regularly, single test files regularly, and the full test suite once at the end.

Commit your work to the current branch, push it, and open a PR.

Ship the PR: /code-review the opened PR in a **different agent** than the
one that wrote the code. Fix every finding it reports — push the fixes to
the same PR, and re-review with a fresh agent until one returns clean.
Merge to main, then sync local main (`git pull --ff-only`).

Propose the next move: read the issue tracker, weigh what this change
unblocked, and name the ticket to pick up next with its reason.
