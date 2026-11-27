# 0004 — Source approval and redistribution

The live allowlist starts empty. Operators may review a repository from any Git
host that meets the [synchronization requirements](../synchronization.md#source-approval).
Review licenses, notices, assets, third-party content, and historical versions
at the revisions in scope before authorizing reads or fetches.

An approval records canonical HTTPS upstream identity, exact ref and path scope, approved history and output forms, reviewer, decision time, policy version, and supporting license and notice evidence. Bound full history to approved refs and evaluate metadata and derived outputs such as search and blame under the same policy. Exclude ambiguous scope until reviewed. Carry notice material alongside source results and preserve original bytes.

Allow revocation to override retained generations and cached contexts. Record the withdrawal, stop new reads under the affected policy, and document treatment of in-flight responses, retirement, and backups. Keep live acquisition disabled until that approval exists.

Phase 4 requires a versioned exploration grant for catalog and object output. Review metadata and approved history alongside each whole-closure, exact-leaf, or subtree grant. A structural ancestor carries its navigation name only when the selected commit contains an approved descendant. The ancestor tree OID is withheld unless that tree is fully approved. [Exploration contract](../exploration.md#output-approval)

Version 2 exploration grants require separate search and history opt-ins. Review
author and committer email addresses alongside other history metadata. Path
grants apply to each side of a diff and each attributed blame line.
[Phase 5 approvals](../mcp.md#approve-output-forms)

Version 3 accepts anchored path globs as standing grants for regular and
executable files on the approved refs. Review the scope across historical and
future matching files. A glob grants navigation through an ancestor when the
selected commit contains an approved descendant. [Glob grants](../exploration.md#output-approval)

During Phase 0, the owner identified `github/docs` and `refs/heads/main` as a
candidate for review. It remains unapproved. Its documentation and code have
separate license terms. [GitHub Docs upstream license summary](https://github.com/github/docs#license)

Rustdoc sources require explicit standing approval for acquisition and
output. A matching source-path and Markdown-path pair grants the derived
document. Git approvals cover the exported workspace and dependency-influenced
prose. A dedicated build guest receives only those job inputs. Hosted
approvals bind an exact release, target, and Rustdoc format.
[Markdown source approval](../markdown.md#approve-and-schedule-sources)
