# Agent instructions

This repository's instructions for coding agents live in [CLAUDE.md](CLAUDE.md). Read it
first; its **Contributing** section is binding. The detailed rules are in skill files. Open
the matching one before starting a task:

| Task | Read |
|------|------|
| Writing, editing or reviewing Rust (tests included) | `.claude/skills/clean-rust/SKILL.md`, then `.claude/skills/clean-rust-filen-rs/SKILL.md` |
| Adding or changing API or binding types | `.claude/skills/types-serde-conventions/SKILL.md` |
| How the Filen server behaves | `.claude/skills/filen-api-behavior/SKILL.md` |
| Writing tests | `.claude/skills/tdd/SKILL.md` |
| Security-sensitive code or dependency changes | `.claude/skills/security/SKILL.md` |
| Committing or reshaping history | `.claude/skills/commit-work/SKILL.md`, `.claude/skills/fold-fixups/SKILL.md` |
| Verifying a change | `.claude/skills/verify-changes/SKILL.md` |
| Checking a finished branch before review | `.claude/skills/house-review/SKILL.md` |

Install the git hooks before your first commit: `bash scripts/git-hooks/install.sh`.
