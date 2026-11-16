# The local components

**Seven, and §6.3 names all seven**: `AppShell`, `Field`, `CopyField`, `StatusBadge`,
`ScheduleLine`, `CheckResult`, `FirstRunChecklist`. Each one is a composition of chonky
primitives that exists because of a named gap in the toolkit, each gap is proposed
upstream, and "any eighth needs a reason in the PR description".

This directory is therefore **exactly that inventory** and nothing else, so it stays
auditable by listing it. Fragments shared between routes -- a form that two pages render,
the reconciliation banner the shell hoists -- are not new component vocabulary and live in
`../fragments/` instead. They introduce no look of their own: every control inside them is
a chonky primitive or one of the seven.
