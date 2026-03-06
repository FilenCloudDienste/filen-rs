---
name: code-style
description: CRITICAL: Always use this skill, no matter what task you are working on!
---

# Code Style Matching Skill

Before writing or modifying **any** code in an existing project, you must analyze and exactly replicate the project's code style. Never impose your own defaults. This skill defines how to do that systematically.

---

## Step 0: Check for Cached Style Index

**Always check for a cached index first** before doing any filesystem scanning. This saves significant tokens on repeated invocations.

```bash
cat ./claude/code-style-index.json 2>/dev/null
```

### If the index exists:

- Load and apply all style rules from it directly — **skip Steps 1–3**
- Proceed to **Step 4 (Resolve Conflicts)** and **Step 5 (Apply Exactly)**
- If you encounter style patterns during your work that are **not covered** by the index, append them (see **Step 6: Update the Index**)

### If the index does not exist:

- Proceed through Steps 1–3 to analyze the codebase
- After analysis, **always create the index** (see **Step 6: Create/Update the Index**)

---

## Step 1: Find and Read Formatter Config Files

_(Skip if index was found in Step 0)_

Immediately scan the project root (and relevant subdirectories) for config files. Read every one you find:

### Formatters & Linters to check

| Tool                  | Config files to look for                                                                                                                                                                                         |
| --------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Prettier**          | `.prettierrc`, `.prettierrc.json`, `.prettierrc.js`, `.prettierrc.ts`, `.prettierrc.yaml`, `.prettierrc.yml`, `.prettierrc.toml`, `prettier.config.js`, `prettier.config.ts`, `"prettier"` key in `package.json` |
| **ESLint**            | `.eslintrc`, `.eslintrc.js`, `.eslintrc.cjs`, `.eslintrc.json`, `.eslintrc.yaml`, `.eslintrc.yml`, `eslint.config.js`, `eslint.config.mjs`, `"eslintConfig"` in `package.json`                                   |
| **Biome**             | `biome.json`, `biome.jsonc`                                                                                                                                                                                      |
| **oxlint / oxc**      | `.oxlintrc`, `oxlint.json`, `oxc.config.json`                                                                                                                                                                    |
| **Rustfmt**           | `rustfmt.toml`, `.rustfmt.toml`                                                                                                                                                                                  |
| **Black / Ruff**      | `pyproject.toml` (`[tool.black]`, `[tool.ruff]`), `ruff.toml`, `.ruff.toml`, `setup.cfg`                                                                                                                         |
| **gofmt / goimports** | (implicit; check for `.editorconfig`)                                                                                                                                                                            |
| **dprint**            | `dprint.json`, `.dprint.json`                                                                                                                                                                                    |
| **Rome**              | `rome.json`                                                                                                                                                                                                      |
| **EditorConfig**      | `.editorconfig` (applies to all languages)                                                                                                                                                                       |
| **TSConfig**          | `tsconfig.json` (affects TS strict settings, not formatting, but useful context)                                                                                                                                 |

**Action**: Run a quick find to locate these files:

```bash
find . -maxdepth 3 \( -name ".prettierrc*" -o -name "prettier.config.*" \
  -o -name ".eslintrc*" -o -name "eslint.config.*" \
  -o -name "biome.json" -o -name "biome.jsonc" \
  -o -name ".editorconfig" -o -name "rustfmt.toml" -o -name ".rustfmt.toml" \
  -o -name "ruff.toml" -o -name ".ruff.toml" -o -name "dprint.json" \
  -o -name "oxlint.json" \) 2>/dev/null | head -30
```

Also check `package.json` for inline config:

```bash
cat package.json 2>/dev/null | grep -A 30 '"prettier"'
cat package.json 2>/dev/null | grep -A 30 '"eslintConfig"'
```

---

## Step 2: Extract Key Style Rules

_(Skip if index was found in Step 0)_

From the config files, extract and note all settings.

---

## Step 3: Study Existing Source Files

_(Skip if index was found in Step 0)_

Even with config files, always verify with real code samples. Pick **all representative files** of the same type you'll be editing:

```bash
# Examples

# For JS/TS projects
find . -name "*.ts" -o -name "*.tsx" -o -name "*.js" | grep -v node_modules | head -1000

# For Rust
find . -name "*.rs" | grep -v target | head -1000

# For Python
find . -name "*.py" | grep -v __pycache__ | head -1000
```

Read the files and check **every one** of these:

### Checklist — observe directly from source

- [ ] **Indentation**: spaces or tabs? how many?
- [ ] **Quotes**: single `'` or double `"`? Template literals `` ` ``?
- [ ] **Semicolons**: present or absent at end of statements?
- [ ] **Trailing commas**: after last item in arrays, objects, params, imports?
- [ ] **Object bracket spacing**: `{ key: value }` or `{key: value}`?
- [ ] **Arrow parens**: `(x) => x` or `x => x`?
- [ ] **Brace style**: opening `{`, `[]` or `(` on same line or new line?
- [ ] **Print width**: approximate line length before wrapping
- [ ] **Import style**: named vs default, grouped, sorted?
- [ ] **Empty lines**: between functions? between class members? at top/bottom of blocks? between variables?
- [ ] **Type annotations** (TS): inline, separate, style of generics spacing
- [ ] **String concatenation vs template literals**
- [ ] **Ternary formatting**: inline or multiline?
- [ ] **Array/object multiline threshold**: when do they break to multiple lines?
- [ ] **Comment style**: `//` vs `/* */`, JSDoc style, spacing after `//`
- [ ] **Export style**: named, default, barrel pattern?
- [ ] **Multiline**: Single-property objects passed as arguments are still expanded to multiple lines: run(fn, {\n\tthrow: true\n}) for better readability
- [ ] **Multiline**: Inline defer callbacks are forbidden — always expand the body: defer(() => {\n\tthis.x.release()\n}) for better readability
- [ ] **Switch cases**: Curly braces for cases for better readability, with a line break between each case

---

## Step 4: Resolve Conflicts

If config and source code disagree, **source code wins** — it reflects what's actually enforced and committed. Config files may be outdated or partially applied.

If different files of the same type show different styles (e.g., some use 2 spaces, some 4), match the **majority pattern** and note the inconsistency. Don't introduce a third style.

---

## Step 5: Apply Exactly

When writing or editing code:

1. **Match every observed convention** — no exceptions, no personal defaults
2. **Never silently "improve" formatting** — if the project uses 4 spaces, use 4 spaces even if you prefer 2
3. **Don't add or remove trailing commas, semis, or parens** unless that's what the project uses
4. **Match empty line patterns** exactly — between functions, before returns, around imports
5. **Match comment style** — if comments use `// ` with a space, don't use `//without`
6. **Match import ordering** — if imports are grouped and sorted, follow the same grouping

---

## Step 6: Create/Update the Style Index

The index lives at `./claude/code-style-index.json`. It is a project-local cache that prevents full codebase re-scanning on future invocations.
If you are in a monorepo, the index file parent directory lives at the project root.

### Creating the index (after first full scan)

After completing Steps 1–3, write all discovered rules to the index:

```bash
mkdir -p ./claude
```

Then write `./claude/code-style-index.json` with this structure:

```json
{
	"_meta": {
		"created": "<ISO date>",
		"updated": "<ISO date>",
		"note": "Auto-generated by code-style skill. Commit this file."
	},
	"indentation": {
		"style": "spaces",
		"size": 2
	},
	"quotes": {
		"js": "single",
		"jsx": "single",
		"ts": "single",
		"tsx": "single",
		"py": "double"
	},
	"semicolons": true,
	"trailingCommas": "es5",
	"bracketSpacing": true,
	"arrowParens": "always",
	"printWidth": 100,
	"endOfLine": "lf",
	"braceStyle": "1tbs",
	"emptyLines": {
		"betweenFunctions": 1,
		"betweenImportGroups": 1,
		"atTopOfBlock": 0
	},
	"imports": {
		"style": "named-preferred",
		"groupOrder": ["builtin", "external", "internal", "relative"],
		"sorted": true
	},
	"comments": {
		"inline": "// ",
		"block": "/* */"
	},
	"exports": {
		"style": "named"
	},
	"notes": []
}
```

Only include keys you actually observed — omit fields you couldn't determine. Add free-form observations to the `"notes"` array (e.g., `"Ternaries always written inline unless > 80 chars"`).

### Updating the index (when new patterns are found)

During any coding session, if you observe a style pattern **not yet recorded** in the index:

1. Read the current index
2. Add or update the relevant field(s)
3. Update `_meta.updated` to today's date
4. Write the file back

Examples of update triggers:

- You notice ternaries are always multiline but `ternaries` isn't in the index → add it
- You see a consistent JSDoc pattern not captured → add to `comments`
- You find an exception for test files (e.g., 4-space indent in `*.test.ts`) → add a `"fileTypeOverrides"` key

**Never remove existing entries** unless they are factually wrong. Add a note instead if there's ambiguity.

---

## Step 7: When Adding New Files

If creating a new file from scratch in an existing project:

- Apply the exact same conventions as sibling files of the same type
- Don't use your own defaults — extrapolate from existing patterns
- If a formatter is configured, write code as if the formatter will be run (i.e., write what the formatter would produce)

---

## Important Reminders

- **Always check `./claude/code-style-index.json` first** — only scan the full codebase if it's missing
- **Always create the index** after a full scan so future invocations are cheaper
- **Always update the index** when you observe uncaptured style rules
- **EditorConfig is language-agnostic** and often overrides tool-specific settings for indentation — always read it
- **Monorepos** may have per-package configs — find the config closest to the file being edited; consider per-package index files (e.g., `./packages/api/claude/code-style-index.json`).
- **Don't run the formatter yourself** unless asked — just write code that would pass formatting unchanged
- **When in doubt, look at more files** — patterns become obvious with 3–5 examples
- **Commit the index file** — it should live in version control so the whole team benefits
