---
name: codebase-search
description: CRITICAL: Always use this skill, no matter what task you are working on!
---

# Codebase Search Skill

When working in any existing codebase, you must actively search for relevant context before writing or answering. Never rely on assumptions about how things are named, shaped, or wired together. This skill defines when and how to search.

---

## Rule: When in Doubt, Search

If any of the following are true, **run a search before proceeding**:

- You are about to call a function, method, or hook you haven't read
- You are about to use a type, interface, or schema you haven't seen defined
- You need to import something and aren't certain of its exact export name or path
- You are implementing a feature that likely has related existing code
- You are asked how something works but haven't read the relevant files
- You see a reference (variable, constant, config key) whose value you don't know
- You are adding to a pattern (e.g. a new route, a new event, a new component) and want to match existing ones
- You are unsure whether something already exists before creating it

**Default posture: search first, then write.** It is always cheaper to run a grep than to write code against a wrong assumption.

---

## Search Toolkit

Use these tools in order of specificity. Prefer faster/cheaper tools first.

### 1. Grep — find by name or pattern

```bash
# Find where a symbol is defined
grep -rn "export.*MyFunction\|export.*MyFunction" --include="*.ts" --include="*.tsx" . | grep -v node_modules

# Find all usages of a symbol
grep -rn "MyFunction" --include="*.ts" --include="*.tsx" . | grep -v node_modules | grep -v ".d.ts"

# Find a type or interface definition
grep -rn "^(export )?(type|interface) MyType" --include="*.ts" . | grep -v node_modules

# Find a config key or constant
grep -rn "MY_CONSTANT\|myConstant" . | grep -v node_modules | grep -v dist

# Case-insensitive search for a concept
grep -rni "upload.*progress\|progress.*upload" --include="*.ts" --include="*.tsx" . | grep -v node_modules
```

### 2. Find — locate files by name

```bash
# Find a file when you know roughly what it's called
find . -name "*auth*" -o -name "*Auth*" | grep -v node_modules | grep -v dist

# Find files by extension in a specific area
find ./src/components -name "*.tsx" | head -20

# Find test files related to a module
find . -name "*.test.ts" -path "*upload*" | grep -v node_modules
```

### 3. Read specific files

Once you've located the right file via grep/find, read it in full or in relevant ranges:

```bash
cat ./src/hooks/useUpload.ts

# Or with line numbers for large files
cat -n ./src/api/routes.ts | head -80
```

### 4. List directory structure

When you need to understand module boundaries or find where something belongs:

```bash
# Two levels deep, ignoring noise
find ./src -maxdepth 2 -type d | grep -v node_modules | sort

# All files in a specific module
find ./src/features/auth -type f | sort
```

---

## What to Search For

### Before using any function or hook

Search for its definition to understand the real signature, return type, and any quirks:

```bash
grep -rn "export.*function useFoo\|export const useFoo" --include="*.ts" --include="*.tsx" . | grep -v node_modules
```

### Before using any type or interface

Read the actual definition — don't reconstruct it from memory:

```bash
grep -rn "interface FooProps\|type FooProps" --include="*.ts" --include="*.tsx" . | grep -v node_modules
```

### Before adding a new instance of a pattern

Find existing instances to match the exact shape:

```bash
# e.g., before adding a new API route handler
grep -rn "router\.(get|post|put|delete)" --include="*.ts" . | grep -v node_modules | head -10
```

### Before creating something that might already exist

Check first:

```bash
grep -rn "formatDate\|format_date" --include="*.ts" --include="*.js" . | grep -v node_modules
```

### Before importing from a module

Verify the export exists and get the exact name:

```bash
grep -rn "^export" ./src/utils/index.ts
```

---

## Search Depth Guidelines

| Situation                                     | Searches needed                                          |
| --------------------------------------------- | -------------------------------------------------------- |
| Using one known, previously-read function     | 0 — already have context                                 |
| Using a function seen earlier in this session | 0 — already in context                                   |
| Using any type/function not yet read          | 1–2 targeted greps                                       |
| Implementing a new feature                    | 3–5 searches across related files                        |
| Refactoring or modifying existing behavior    | Read all directly affected files first                   |
| Answering "how does X work"                   | Read the relevant files, don't summarize from assumption |

---

## Anti-Patterns to Avoid

**Never do these:**

- ❌ Invent a function signature because it "seems right"
- ❌ Assume an import path without verifying it exists
- ❌ Reconstruct a type from context instead of reading its definition
- ❌ Assume a pattern is consistent without checking a real example
- ❌ Write code that calls into modules you haven't read
- ❌ Answer "does X exist?" without searching

**Always do these instead:**

- ✅ Grep for the symbol, read the definition, then use it
- ✅ Find one or two real usages before writing your own
- ✅ Check the actual export list before importing
- ✅ Read the file you're editing from top to bottom before changing it
- ✅ If a definition, type, function, variable etc. comes from a third party dependency, look it up in the dependency directory (e.g. node_modules)

---

## Efficient Search Habits

- **Combine greps**: search for both the definition and a usage in one pass to orient yourself fast
- **Read before editing**: always `cat` the full file you're about to modify, not just the target lines
- **Follow imports**: if a file imports something unfamiliar, check what it is before using it
- **Check barrel files**: many projects re-export from `index.ts` — check these for the canonical import path
- **Scope searches**: use `--include` and path filters to avoid noise from `node_modules`, `dist`, `__pycache__`, etc.

---

## Noise Filters (use consistently)

```bash
# Standard exclusions for most projects
grep -rn "..." . \
  | grep -v node_modules \
  | grep -v "/dist/" \
  | grep -v "/.next/" \
  | grep -v "__pycache__" \
  | grep -v ".d.ts" \
  | grep -v "/coverage/"
```

Or use `--exclude-dir` for cleaner commands:

```bash
grep -rn "..." \
  --include="*.ts" \
  --exclude-dir=node_modules \
  --exclude-dir=dist \
  --exclude-dir=.next \
  .
```

---

## Reminder

Code that is written without reading the codebase first is a guess. Guesses break things. The codebase is always the source of truth — search it.
