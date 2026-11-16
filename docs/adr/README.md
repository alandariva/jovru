# Open decision proposals

This folder holds only **proposals**: structural decisions that are written
down but not yet accepted. A decision that is accepted does not stay here. Its
substance moves into the document of its area, and the proposal file is
deleted (git keeps the history).

Decisions already made live in [decisions/](../decisions/README.md), one file per
area, numbered `D-NNNN`.

## When a decision needs a proposal

A change needs one when it affects more than one crate, a core contract, the
persistence schema or the stack. A smaller choice is written straight into the
document that owns it. Renaming something, or choosing between two ways of
doing a task inside one module, needs no proposal.

## How a proposal becomes a decision

1. Write `NNNN-short-title.md` from the [template](0000-template.md), either on
   its own or with the code that motivates it. The number is the next unused
   one (the last decision is D-0045, so the next proposal is 0046). Agents may
   write proposals; their status is *Proposed*.
2. Only the maintainer accepts a proposal (*Proposed* → *Accepted*).
3. Once the proposal is accepted, fold it into the owning document:
   - Move the rules, contracts, limits and names it establishes into the
     body of that document, where they stay authoritative.
   - Add an entry to the `decisions/` file of that area, in this shape, using
     the proposal's number:

     ```markdown
     <a id="d-0043"></a>
     ## D-0043: Title

     *Accepted 2026-11-12 · implemented | not implemented | partly* · Amends D-0002

     **Decision.** What was decided, in a few sentences, pointing to the section
     that holds the detail.
     **Why.** The forces and reasoning that the code does not show.
     **Rejected.** Each alternative and why it lost.
     ```
   - Add the row to the [decision index](../decisions/README.md)
     and delete the proposal file.
4. To change a decision later, edit its entry and the body it points to, and
   say what changed and when. If the change is itself structural, write a new
   proposal that amends it.

## Index

No proposals are open.
