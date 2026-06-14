# Agent Instructions

This project uses **Linear** for issue tracking. Issues are managed via the Linear MCP integration (tools prefixed with `mcp__plugin_linear_linear__`).

## Non-Interactive Shell Commands

**ALWAYS use non-interactive flags** with file operations to avoid hanging on confirmation prompts.

Shell commands like `cp`, `mv`, and `rm` may be aliased to include `-i` (interactive) mode on some systems, causing the agent to hang indefinitely waiting for y/n input.

**Use these forms instead:**
```bash
# Force overwrite without prompting
cp -f source dest           # NOT: cp source dest
mv -f source dest           # NOT: mv source dest
rm -f file                  # NOT: rm file

# For recursive operations
rm -rf directory            # NOT: rm -r directory
cp -rf source dest          # NOT: cp -r source dest
```

**Other commands that may prompt:**
- `scp` - use `-o BatchMode=yes` for non-interactive
- `ssh` - use `-o BatchMode=yes` to fail instead of prompting
- `apt-get` - use `-y` flag
- `brew` - use `HOMEBREW_NO_AUTO_UPDATE=1` env var

## Issue Tracking with Linear

**IMPORTANT**: This project uses **Linear** for ALL issue tracking. Do NOT use beads (`bd`), markdown TODOs, or other tracking methods.

### Workflow for AI Agents

1. **Find work**: Use `mcp__plugin_linear_linear__list_issues` to find assigned/unstarted issues
2. **Understand context**: Use `mcp__plugin_linear_linear__get_issue` to read issue details
3. **Work on it**: Implement, test, document
4. **Discover new work?** Create a linked issue via `mcp__plugin_linear_linear__save_issue`
5. **Update status**: Move issues through statuses as work progresses via `mcp__plugin_linear_linear__save_issue`

### Important Rules

- Use Linear for ALL task tracking (via MCP tools)
- Check Linear issues before asking "what should I work on?"
- Do NOT use `bd` (beads) — it is deprecated in this project
- Do NOT create markdown TODO lists for persistent tracking
- Do NOT duplicate tracking systems

## Code Review Bots (PR review process)

PRs are reviewed by automated bots in a **strict order**. Do not run them out of order or trigger them eagerly.

1. **Qodo and CodeAnt — first pass (automatic).** Both run automatically on every push; no trigger needed. Read all three comment surfaces: inline review threads, PR issue-comments (Qodo/CodeAnt post findings here), and PR reviews.
2. **Address everything from Qodo + CodeAnt.** Bundle fixes locally and push once per round (avoid a flurry of tiny pushes). Each push re-runs Qodo + CodeAnt + CI. Repeat until they are clean.
3. **CodeRabbit — final pass (manual, once).** Org auto-review is **off**; CodeRabbit runs only when you comment `@coderabbitai review`. Trigger it **exactly once, after Qodo + CodeAnt have converged** on the final code — never before (it would review code that is about to change) and never while it is inside a rate-limit window. It is the last gate.

**Cursor / Bugbot:** presumed **unavailable** (moved to usage-based pricing). Do not wait for it or expect comments; it returns only if the maintainer re-enables it at their discretion.

**Greptile:** maintainer-triggered only. **Never** trigger it and **never** write the literal `@greptile` in a comment — the at-mention itself starts a billed run. The PR author is in Greptile's excluded-authors list, so it auto-skips.

**Converged** = CI green **and** Qodo + CodeAnt clean **and** the single CodeRabbit pass addressed (the end of available feedback). At that point the PR is ready for human merge review. **Never auto-merge** — the maintainer merges.

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create Linear issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Update Linear issue statuses as appropriate
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
