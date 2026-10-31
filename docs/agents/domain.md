# Domain docs

Before exploring an area, read the root `GLOSSARY.md` and relevant
decisions in `docs/adr/`, when they exist. Proceed silently if they do
not exist; domain-modeling creates them when terms or decisions are
actually resolved.

This repository uses a single context:

- `GLOSSARY.md`: shared domain vocabulary.
- `docs/adr/`: architectural decision records.

Use glossary terms when naming domain concepts in issues, proposals,
hypotheses, and tests. If a needed concept is missing, reconsider
whether the project uses it or note the gap for domain-modeling.

When a proposal contradicts an existing ADR, surface the conflict
explicitly and identify the decision that would need reopening.
