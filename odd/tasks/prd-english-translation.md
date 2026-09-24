# Feature: PRD English translation + D-1 resolution

## Objective
Translate `PRD.md` (765 lines, v2.0) from Spanish to English in full, and apply
the resolved product decision D-1 (MCP contract entirely in English).

## Problem
`PRD.md` is the project's single source of truth and is written in Spanish,
while the repository, project name and README are in English. The owner decided
that documentation and every other project artifact is English; conversation
stays in Spanish. D-1 (MCP contract language) was left open in §20 and blocked
RF-15.

## Why
Consistency across the repository, interoperability of the MCP surface with the
wider ecosystem (English `snake_case` tool names are the de-facto convention),
and removal of a blocking open decision before Phase 1 starts.

## Scope
Authorized: `PRD.md` only, plus this task document and its Engram mirror.
Out of scope: any source code, `.gitignore`, README creation, or resolving
D-2..D-5.

## Constraints
- Preserve every identifier verbatim: RF-1..RF-64, RNF-1..RNF-13, D-1..D-5,
  phase names, annex letters, table structure and heading numbering.
- Preserve Markdown structure: 56 headings, same order, same nesting.
- Do not add, remove or reinterpret requirements. Translation only, except for
  the D-1 changes explicitly listed in T6.
- Technical terms stay in their canonical English form; code identifiers,
  SQL, property names and crate names are never translated.

## TDD mode
Not applicable — documentation-only change, no test runner involved.
Resolved from: repository state (no code exists yet; repo contains only
`PRD.md` and `.gitignore`).
Applicable checks are structural, listed under Acceptance criteria.

## Tasks
- [x] T1 — Translate lines 1–143: title, §1 Summary .. §10 User stories
      Evidence: Current prose-only scan of lines 1–143 after removing inline code literals found 0 accented-character matches and 0 Spanish-identifier matches; the current ledger records 13 headings in the span.
      Commit: `f5cdc80` (translation); current evidence wording is recorded in `75ff044`.
- [x] T2 — Translate lines 144–313: §11.1 Capture .. §11.3 Storage
      Evidence: Current raw scan found four accented `ó` characters, all inside preserved RF-48 regex/code literals; the prose-only scan after removing inline code literals found 0 accented-character matches and 0 Spanish-identifier matches across 5 headings.
      Commit: `f5cdc80` (translation); current evidence wording is recorded in `75ff044`.
- [x] T3 — Translate lines 314–447: interval clipping, §11.4 MCP, §11.5 CLI,
      §11.6 Service, §12 NFRs, token budget
- [x] T4 — Translate lines 448–598: §13 Architecture, §14 Privacy and threat
      model, §15 Test strategy
- [x] T5 — Translate lines 599–765: §16 Risks, §17 Phases, §18 Success metrics,
      §19 Packaging, §20 Open questions, Annexes A/B/C
- [x] T6 — Apply D-1: RF-15 tool names to English `snake_case` with English
      descriptions; mark D-1 resolved in §20 with the decision and its date
- [x] T7 — Verify: heading count, identifier inventory, no Spanish residue
      Evidence: Current structural checks found 56 headings with nesting identical to `f5cdc80^`, complete RF-1..RF-66/RNF-1..RNF-13 inventories with no missing or dangling references, and D-1 resolved.
      Commit: `7bc22e6` (current ledger evidence); wording correction is recorded in `75ff044`.

## Acceptance criteria
- `PRD.md` contains no Spanish prose.
- `grep -c '^#' PRD.md` returns 56, same heading order as before.
- Full inventory RF-1..RF-64 and RNF-1..RNF-13 present, no gaps, no dangling
  references.
- RF-15 exposes English tool names; §20 records D-1 as resolved.
- `git diff --stat` touches only `PRD.md` and `odd/`.

## Progress
Created 2026-09-17. All tasks T1–T7 complete.

Historical verification evidence (observed, 2026-09-17; not replayed verbatim):
- `grep -c '^#' PRD.md` → 56, same as before the change.
- `diff <(grep -o '^#*' PRD.md.orig) <(grep -o '^#*' PRD.md)` → identical heading
  nesting sequence, 56 entries, same order.
- RF-1..RF-64 and RNF-1..RNF-13 inventory loop → no missing identifier.
- Spanish function-word and accented-character scan outside regex literals →
  no match.
- Spanish identifier scan (`resumen`, `regla_*`, `oculto`, `pendiente`, ...) →
  no match; the 11 English tool names are present.
- `wc -l PRD.md` → 765 lines, identical to the pre-translation file.
- `git status --short` → only ` M PRD.md` plus untracked `odd/`.

## Current ledger evidence (observed 2026-09-23)

The block above is retained as provenance, not as a current claim. `PRD.md.orig`
is absent, so its original diff and residue-scan outputs are
**historical/unverifiable as a replay**. The PRD also changed after `f5cdc80`:
`git diff --stat f5cdc80..HEAD -- PRD.md` reports 79 changed lines, and the
current file is 800 lines. The following read-only checks were run against the
current `PRD.md`.

| Task | Ledger status | Current evidence |
|---|---|---|
| T1 | **PROVEN — current structural evidence** | Prose-only scan of current lines 1–143 after removing inline code literals: 0 accented-character matches and 0 Spanish-identifier matches; 13 headings in the span. |
| T2 | **PROVEN — current structural evidence** | Raw scan of current lines 144–313 finds 4 accented `ó` characters: two on line 241 (`navegaci[oó]n`, `inc[oó]gnito`) and two on line 243 (`c[oó]digo`, `verificaci[oó]n`). All four are inside preserved RF-48 inline regex/code literals; the prose-only scan after removing inline code literals has 0 accented-character matches and 0 Spanish-identifier matches; 5 headings in the span. |
| T7 | **PROVEN — current structural evidence** | `wc -l PRD.md` → 800; `grep -c '^#' PRD.md` → 56; heading nesting vs `f5cdc80^` → identical (56/56); current RF-1..RF-66 and RNF-1..RNF-13 inventories → no missing identifiers or dangling references; D-1 → resolved. |

The current inventory includes RF-65 and RF-66, which were added after the
translation commit. No claim is made that the historical translation review
was replayed semantically; the statuses above close T1, T2 and T7 on the
reproducible structural evidence available in the current file.

Deliberate content changes beyond pure translation, all flowing from D-1:
- RF-15 tool, parameter and alias names moved to English `snake_case`.
- `rules.status` CHECK values in RF-11 moved to 'pending'/'confirmed'/
  'rejected'/'inactive'.
- The `[oculto]` placeholder title became `[hidden]` (RF-8, RF-47, §14.3, §17).
- The `"(escritorio)"` app_id sentinel became `"(desktop)"`.
- §12 token budget figures recomputed against the renamed English JSON example:
  176 chars ≈ 50 tokens/row (was 182 ≈ 52), 200 rows ≈ 10,000 (was 10,400),
  total ≈ 11,300 (was 11,700); compact row 63 chars ≈ 18 tokens, 64 % less
  (was 65 ≈ 19, 63 %). Every conclusion and threshold is unchanged.
- Spanish alternatives inside the RF-48 default-exclusion regexes were kept
  verbatim: they match real window titles and removing them would change
  runtime behavior, not language.

## Next step
D-2 was resolved on 2026-09-23 as one `xwindowlog` binary with the shared
`panic = "unwind"` release profile. D-4 was resolved the same day as wlroots-only
evaluation in v2. D-5 was resolved the same day in favor of minisign: the official
release contract is a sorted `SHA256SUMS` manifest signed with minisign, with the
public key and fingerprint published for verification. Provisioning the signing
key and documenting the verification material remain prerequisites before the
first release; no key, fingerprint, release workflow, or automation is claimed
here. Then Phase 1 (daemon); acceptance criteria are already written in §17.
