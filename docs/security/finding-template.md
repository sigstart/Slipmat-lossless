# Security finding template

Copy for each candidate. Use source citations tied to the reviewed revision.
Quote only the relevant lines and redact private data and credentials.

## [ID] [Component and concrete impact]

| Field | Value |
| --- | --- |
| Status | Candidate / Confirmed / Dismissed / Needs evidence |
| Severity | Critical / High / Medium / Low / Informational / Pending |
| Confidence | High / Medium / Low, with rationale |
| Checkout and revision | Repository path, full commit ID, and relevant worktree differences |
| Affected configurations | Platform, package, and runtime configuration verified or assumed |
| Reviewer | Review owner, model and effort if applicable, and second reviewer if used |

### Boundary and prerequisites

- Actor and starting access
- Protected asset or privilege and the boundary it depends on
- Deployment, permissions, account state, user interaction, and other required conditions

### Source evidence

| Step | Revision and `path:line` | What the source establishes |
| --- | --- | --- |
| Input | | Actor-controlled value and the entry point that receives it |
| Flow | | Parsing, transformation, and validation across process or language boundaries |
| Operation | | Security-sensitive sink and the conditions required to reach it |
| Existing defenses | | Access checks, isolation, limits, and counterevidence |

Add short redacted excerpts only where they clarify the path. For advisory
evidence, include the advisory identifier, resolved component and version,
affected range, source and date, and evidence for the shipped configuration.

### Impact and disposition

- Concrete consequence under the stated assumptions and reason for severity
- Methods used to validate the path and limits of the evidence
- Reason for the current status, with the exact missing fact for unresolved candidates
- Smallest remediation that addresses the cause and the expected defensive behavior

### Coverage

- Checked paths, interfaces, and configurations
- Untested configurations, external dependencies, and other gaps
