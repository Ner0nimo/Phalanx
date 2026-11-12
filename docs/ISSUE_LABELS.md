# GitHub Issue Labels — Phalanx

This file defines the intended issue-label taxonomy. Keep it deliberately small: use GitHub's built-in type labels for issue kind, custom labels for priority/area/status, and the `[M0]`–`[M5]` title prefix for the roadmap phase until repository milestones are configured.

## Built-in labels

Retain GitHub's standard labels where available:

| Label | Use |
|---|---|
| `bug` | Incorrect behavior or regression |
| `enhancement` | Feature, implementation task, or technical improvement |
| `documentation` | Documentation, design specification, or ADR |
| `question` | A decision or clarification is needed |
| `help wanted` | Work is suitable for external contribution |
| `good first issue` | Small, isolated task suitable for a newcomer |

Avoid applying `good first issue` to foundational architecture work.

## Custom labels to create

### Priority

| Label | Color | Meaning |
|---|---|---|
| `priority:P0` | `b60205` | Blocks the next milestone or risks a costly architectural mistake |
| `priority:P1` | `d93f0b` | Important for the current milestone; should be scheduled soon |
| `priority:P2` | `fbca04` | Normal planned work |
| `priority:P3` | `0e8a16` | Low urgency, polish, or optional improvement |

### Area

| Label | Color | Meaning |
|---|---|---|
| `area:architecture` | `5319e7` | Architecture, ADRs, subsystem boundaries |
| `area:simulation` | `1d76db` | Tick loop, world state, determinism, replay |
| `area:gameplay` | `0e8a16` | Orders, combat, economy, territory rules |
| `area:navigation` | `0052cc` | Pathfinding, movement, formations, avoidance |
| `area:logistics` | `fbca04` | Supply, resources, production, reinforcement |
| `area:rendering` | `c2e0c6` | Renderer, GPU, camera, visual presentation |
| `area:ai` | `d4c5f9` | Strategic, operational, and tactical AI |
| `area:editor-api` | `bfd4f2` | Editor, agent commands/queries, map tools |
| `area:infrastructure` | `e4e669` | CI, build, packaging, dependency/tooling |

### Workflow

| Label | Color | Meaning |
|---|---|---|
| `status:blocked` | `b60205` | Cannot proceed until a dependency or decision is resolved |
| `status:needs-design` | `d876e3` | Requires a product/technical design decision before implementation |
| `status:needs-prototype` | `d4c5f9` | Requires a measured technical spike before commitment |

Do not add a separate `status:ready` label: open issues without a blocking status are ready to be triaged.

## Usage rules

1. Apply exactly one built-in type label.
2. Apply exactly one priority label once priority has been triaged.
3. Apply one or two area labels at most.
4. Use a workflow label only when it adds information; remove it when the blocker/design task is resolved.
5. Keep the phase in the title as `[M0]`, `[M1]`, etc. Prefer actual GitHub milestones once they are configured.
6. Revisit priority during milestone planning; priority is not a permanent property of an issue.

## Create the custom labels

The connected GitHub actions available to this session can create issues and assign existing labels, but do not expose the repository label-creation endpoint. Once the repository labels are managed from GitHub or GitHub CLI, the following commands create the custom labels defined above. Run them from a machine authenticated to this repository with permission to manage labels.

```bash
gh label create "priority:P0" --color "b60205" --description "Blocks the next milestone or risks a costly architectural mistake" --force
gh label create "priority:P1" --color "d93f0b" --description "Important for the current milestone; schedule soon" --force
gh label create "priority:P2" --color "fbca04" --description "Normal planned work" --force
gh label create "priority:P3" --color "0e8a16" --description "Low urgency, polish, or optional improvement" --force

gh label create "area:architecture" --color "5319e7" --description "Architecture, ADRs, subsystem boundaries" --force
gh label create "area:simulation" --color "1d76db" --description "Tick loop, world state, determinism, replay" --force
gh label create "area:gameplay" --color "0e8a16" --description "Orders, combat, economy, territory rules" --force
gh label create "area:navigation" --color "0052cc" --description "Pathfinding, movement, formations, avoidance" --force
gh label create "area:logistics" --color "fbca04" --description "Supply, resources, production, reinforcement" --force
gh label create "area:rendering" --color "c2e0c6" --description "Renderer, GPU, camera, visual presentation" --force
gh label create "area:ai" --color "d4c5f9" --description "Strategic, operational, and tactical AI" --force
gh label create "area:editor-api" --color "bfd4f2" --description "Editor, agent commands/queries, map tools" --force
gh label create "area:infrastructure" --color "e4e669" --description "CI, build, packaging, dependency/tooling" --force

gh label create "status:blocked" --color "b60205" --description "Cannot proceed until a dependency or decision is resolved" --force
gh label create "status:needs-design" --color "d876e3" --description "Requires a product or technical design decision before implementation" --force
gh label create "status:needs-prototype" --color "d4c5f9" --description "Requires a measured technical spike before commitment" --force
```
