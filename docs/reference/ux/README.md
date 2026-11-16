# User experience

This reference describes what the user sees and does; how GPUI code is written to deliver it is in the [UI guidelines](../../standards/ui-guidelines.md).

Jovru should feel immediate: every action answers at once, every state
is visible, and the keyboard reaches everything.

## Files

- [`layout.md`](layout.md): The window, its panels and navigation.
- [`states.md`](states.md): What each view shows in every state.
- [`shortcuts.md`](shortcuts.md): The default keys and how the user changes them.
- [`feedback.md`](feedback.md): Feedback, confirmations, error messages, text, theme, motion and accessibility.

## Behaviours to preserve

A behavioural audit of 47 findings (November 2026) left these as regression
guards; a change that breaks one needs a reason recorded here.

- **Cancellation blocks new runs** in the same tab (Stopping…), so two sessions never compete.
- **Open-cursor pagination** never reruns expensive or nondeterministic SQL, and closed cursors are disclosed.
- **Per-tab sessions with honest notices** when schema or temporary-object context resets.
- **Complete server errors:** classified title, original message, SQLSTATE, position, Copy details, and Go to the position without selecting SQL that the next keystroke could erase.
- **Moving a tab between connections asks first and keeps its SQL**; closing SQL outside the library asks first.
- **Passwords never migrate between servers** (`keeps_password` matches driver, host, port and user only).
- **Faithful values:** NULL distinct from empty and spaces, exact decimals, no time-zone conversion, line breaks shown as ↵.
- **Click selects, double-click or Enter opens**, in the explorer and the palette; a right-click outside the selection selects the cell first.
- **Per-tab timeout visible in the toolbar**; the per-statement gutter follows the server (Pending → Running → Success/Error).

The findings came from six patterns worth checking in new code: controls whose
repetition reverses their meaning (run cancels, save removes); identity by name
instead of stable IDs; results or context detached from the statement that
produced them; demo code on real paths; state changes without a dialog; and key
handling outside the keymap.

## Known deviations

| Standard | Current code |
| --- | --- |
| Error position marked in the editor | MySQL marks only syntax errors (an unknown column or a missing table is not), and a MySQL or Databricks cluster statement with `:name` parameters is not marked, since the driver changed its text. PostgreSQL's position with `:name` parameters lands off by the difference in length when a parameter comes before the error |
