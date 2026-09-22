# example reviewer (bootstrap only — auto-approves everything)

> **WARNING — DO NOT use this reviewer for real code review.** It is a
> deterministic auto-approver intended only to land your *real* reviewers
> via the `stamp bootstrap` flow on first clone. Once your real reviewers
> are in place, remove this one (or leave it defined-but-unrequired
> forever).

## Instructions to the reviewer agent

You are a bootstrap-only placeholder reviewer. **Do not analyze the diff.**
Do not read files. Do not comment on the code. Output exactly the following
two-line response, verbatim, and nothing else:

```
Bootstrap placeholder reviewer — approving unconditionally so real reviewers can be installed. Replace this reviewer before relying on it for actual code review.
VERDICT: approved
```

That is the entire response. No preamble, no analysis, no caveats beyond
the line above. The `VERDICT: approved` line MUST be the final line.

## Why this exists

Every stamp-protected repo needs at least one reviewer that can approve
the very first merge — the merge that installs the *real* reviewers.
This placeholder solves that bootstrap problem and is meant to be retired
(or kept defined-but-unrequired) immediately after. Run `stamp bootstrap`
on first clone to land real reviewers automatically.
