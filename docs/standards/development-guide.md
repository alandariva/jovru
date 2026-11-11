# Development guide

Keep changes local to the part of Jykre Studio that owns the behavior.
A useful boundary lets a rule change without requiring edits to unrelated
features. Use the same boundaries when adding a feature or refactoring an
existing one.
[Golden rules](../README.md#golden-rules) · [Workflow](workflow.md)

## Put each decision where it belongs

| Responsibility | Where it belongs |
| --- | --- |
| Render state, manage focus and translate input into actions | GPUI views under `src/app/`. Keep view state with the interaction that owns it. [UI guidelines](ui-guidelines.md) |
| Apply feature rules independent of GPUI and orchestrate operations | The relevant module of the `jykre` library. Follow `backend::run` for I/O and the existing feature's task ownership. [Architecture](../reference/architecture.md#layers-and-responsibilities) |
| Execute database-specific SQL, interpret protocol errors or expose a database capability | The driver crate. Consumers use the existing descriptor, capabilities or dialect as appropriate. [Drivers](../reference/drivers.md) |
| Store local state or access credentials | Use `jykre-storage` for SQLite repositories and `jykre-secrets` for the OS vault. [Persistence](../reference/persistence.md) · [Security](security.md#credentials) |
| Define a shared driver contract or pure type that crosses crate boundaries | Follow the role of `jykre-core` and the dependency rules. Feature-local types belong with their feature. [Architecture](../reference/architecture.md#dependency-rules) |

Keep a rule and the data it governs together. Give each mutable state one
owner and derive counts, labels and flags from it. At a boundary, pass the
inputs the caller can name and return an outcome the caller can act on.
Keep dependencies pointed toward the layer that owns the rule. Keep
implementation details private and expose the operations callers need.
[Dependency rules](../reference/architecture.md#dependency-rules)

Start with a concrete function or type in the existing module. Extract a
private submodule when it gives a responsibility a clear name and a small
surface. A long file can reveal several responsibilities. Judge the split
by how many callers and pieces of state a future change has to touch.
Consider a trait when real implementations need to vary behind a shared
contract. Consider a crate when a boundary needs enforcement by the
dependency graph. Follow the approval process for structural changes and
propose a dependency when it would cost less to maintain than local code.
[Workflow](workflow.md#architecture-decisions) · [Dependency policy](stack-and-dependencies.md#policy-for-new-dependencies)

Use names that describe the action and its result. Prefer explicit states and
typed errors at boundaries so callers can handle outcomes consistently.
Keep comments for invariants, lifecycle constraints and decisions a reader
could otherwise miss. [Rust guidelines](rust-guidelines.md)

Follow the data through the feature. Keep I/O and work that grows with the
input off the UI thread. Use the existing paging and memory limits when
reading results, and share result data across consumers. Recompute derived
data when its inputs change and let each frame render the visible portion.
[Performance principles](performance.md#principles) · [Result limits](../reference/results.md#limits)

## Design and build a new module

1. **Describe the behavior.** Identify the action that starts it, the
   expected result and what the caller sees on invalid input or failure.
   For stateful work, identify who owns the state and what happens when
   the caller closes, cancels or starts the action again.
2. **Choose its home and interface.** Extend the existing owner if the
   feature changes a rule it already owns. Create a module when the new
   behavior has a distinct responsibility. Use the project's existing
   types and APIs where their guarantees fit. Keep the interface small
   enough to test through its inputs and outcomes.
3. **Connect one complete flow.** Wire the module into the normal action,
   command or service that calls it. Keep I/O behind the existing
   boundaries. For work that can fail partway, decide when to publish
   state and what must be cleaned up. For features that read saved data,
   account for files and settings written by earlier versions.
   [Concurrency](../reference/architecture.md#concurrency) · [Reliability](reliability.md#failure-isolation) · [Persistence](../reference/persistence.md)
4. **Cover the contract.** Use unit tests for pure rules and isolated
   integration tests for interactions with storage, drivers or external
   services. Use synthetic data for the normal path and relevant failure
   cases. Exercise the real caller for behavior that depends on wiring,
   focus or the lifecycle of a view. [Testing](testing.md) · [User experience](../reference/ux.md)

### Example from the SQL editor

**Go to line** takes text and a line count. The library's `go_to_line`
module parses the target and calculates the cursor position. Its tests
cover invalid and out-of-range input without a window. The GPUI adapter
owns the prompt and focus, and passes the target to the editor, which
provides the line count without scanning the document. The `editor`
performance scenario exercises the flow on a large SQL file.
[Library module](../../src/go_to_line.rs) · [Editor adapter](../../src/app/sql_editor/go_to_line.rs) · [Performance run](performance.md#automated-performance-run)

## Refactor in steps that preserve behavior

Look for concrete evidence of a boundary worth improving. A common change
may touch several unrelated areas, a rule may have conflicting copies,
or a measured path may exceed its budget. Use file size to find candidates,
then compare how often the code changes, the impact of regressions and the
smallest responsibility you can verify on its own. Consult existing
decisions and known deviations before proposing work on accepted debt.
[Architecture decisions](workflow.md#architecture-decisions) · [Technical debt](../plans/technical-debt.md)

1. **Identify what must keep working.** Find the callers, the state owner,
   and the documented behavior. Look for existing tests and record the
   baseline for measured paths. If an important behavior lacks coverage,
   capture it through an existing entry point before moving the code.
   [Testing](testing.md#rules-for-changes-and-review) · [Performance](performance.md#how-to-measure)
2. **Move one responsibility at a time.** Start where the inputs and outputs
   are already clear. Preserve entry points and keep the move reviewable
   separately from behavior changes. Reuse the existing types and helpers.
   Keep state and the methods that enforce its invariants close together.
3. **Check ownership and failure paths.** Trace who owns in-flight work, when
   derived data is invalidated and what callers see on failure. For async
   work, preserve cancellation, deadlines, cleanup and protection against
   stale results.
   For persisted data, check older data and what remains visible after
   failure or cancellation. Use the documented approval and migration
   process if a schema change is needed. [Concurrency](../reference/architecture.md#concurrency) · [Reliability](reliability.md#failure-isolation) · [Migrations](../reference/persistence.md#migrations)
4. **Review the result of the split.** Confirm that a change to the extracted
   rule or interaction can now be made within its owning module. Check
   that callers use the intended interface and that the split has kept
   ownership clear. Rerun the original scenarios through the same entry
   points before taking on the next responsibility.

### Example of refactoring the result grid

`src/app/result_grid.rs` contains rendering, selection, column search,
editing and copy actions. It is also the shared view for results outside the
query workspace. Keep `ResultGrid` as the cached GPUI entity, with its
existing methods and events for callers. Split responsibilities inside it
first. A new GPUI entity is useful when a region changes independently and
can avoid redrawing expensive neighbors. [Result grid](../../src/app/result_grid.rs) · [ADR-0012](../adr/0012-result-grid-as-a-cached-entity.md) · [GPUI rendering](ui-guidelines.md#how-gpui-renders-and-what-follows-from-it)

1. **Isolate column search.** The private `result_grid/column_search.rs`
   module holds the dialog state, keyboard handling and match rendering.
   It uses `jykre::column_search` for name matching and the column layout
   API to reveal the chosen column. Preserve recomputation on text,
   layout or result changes. In the app, open it with `Ctrl Shift F`,
   navigate with the arrows and choose a hidden column with `Enter`. Open
   it again and check that `Esc` returns focus to the grid. Run the
   `columns` performance scenario on the same fixture before and after.
   [Grid behavior](../reference/results.md#grid) · [Keyboard shortcuts](../reference/ux.md#keyboard-shortcuts) · [Performance run](performance.md#automated-performance-run)
2. **Separate other interactions as follow-up changes.** Keep selection
   gestures, focus and scrolling in the view, using `jykre::grid` for
   selection calculations. Keep the cell editor and its input handling
   together and reuse `grid_edit::EditSession` for pending edits. Let grid
   copy actions build a request for the existing `app::copying` flow and
   keep format rules in `jykre::copy_as`. Exercise drag and keyboard
   selection after a filter or sort, pending edits when switching result
   tabs, and copying a selection through its normal entry point.
   [Grid implementation](../../src/grid.rs) · [Editing](../reference/results.md#editing) · [Copying](../reference/results.md#copying)
3. **Extract table rendering in its own step.** Preserve the cached grid
   boundary, visible-row and visible-column rendering, cache invalidation
   and targeted notifications. Use the `grid` and `edit` performance scenarios
   for changes to their respective paths. Verify the shared grid in affected
   callers, including report panels when shared rendering changes. [Performance run](performance.md#automated-performance-run)

## Verify the change

- Review the diff against the intended behavior, the crate dependency rules
  and the relevant security requirements. Update the reference when
  documented behavior changes. [Review checklist](workflow.md#review-checklist)
- Open the app for changes to visual flows and check keyboard and focus.
  On measured paths, compare release builds with the same fixture and
  conditions. Report the before and after against the applicable budgets.
  [UI guidelines](ui-guidelines.md) · [Performance budgets](performance.md#budgets)
- Run `cargo xtask check` for code changes. Run `cargo xtask check --full`
  for driver, execution contract and dependency changes, and for release
  preparation. Changes to `Cargo.toml` or `Cargo.lock` require
  `cargo deny --locked check`, which full mode includes. Keep regression
  tests and doctests intact.
  [Verification gate](workflow.md#verification-gate)
- Report what passed, failed or was not run, with reasons and any ignored
  tests. Record the visual check separately and include measurements for
  paths covered by a budget. [Task report](workflow.md#task-report)
