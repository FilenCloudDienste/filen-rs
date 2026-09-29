## What and why

<!-- What changed and why, and where a reviewer should start. -->

## Checklist

- [ ] The git hooks are installed, and pre-commit and pre-push ran without `--no-verify`.
      `SKIP_*` variables used, and why:
- [ ] The `house-review` skill was run over the branch; its findings are fixed or answered
      below.
- [ ] Follow-up fixes are folded into the commits they correct; no `fixup!`, `squash!` or
      `amend!` commits remain.
- [ ] Fixes to pre-existing code and behaviour changes to shipped features are standalone
      commits at the front of the branch.
- [ ] Every commit hash cited in a message or in this description exists on this branch or
      on `main`.
- [ ] No `unsafe`, `vendor/`, `[patch]`, key wiping, binary file or git dependency from
      outside the maintainer's GitHub accounts went in without the maintainer's sign-off
      (link it).
- [ ] New dependencies and new major versions of existing ones, if any, are listed below
      with the reason.
- [ ] The live test suites this change touches were run, or are named below as not run.

## Not run, open questions
