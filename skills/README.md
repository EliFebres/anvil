# skills

Skills that ship with forge. None exist yet.

Each skill is a directory `skills/<name>/SKILL.md` in Claude Code's skill format, so the same files load unchanged in Claude Code:

```markdown
---
name: <kebab-case name, matches the directory>
description: <one paragraph: what the skill does and when to use it>
allowed-tools: Read, Grep   # optional, comma-separated
---

Instructions for the model follow as Markdown.
```

The frontmatter is parsed into `forge_types::SkillManifest`; `name` must be kebab-case and at most 64 characters, `description` at most 1024.
