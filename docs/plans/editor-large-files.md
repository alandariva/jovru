# Large `.sql` files in the editor: the D7 limit

- **Approval:** Approved by the maintainer on 2026-11-08: option A with C,
  the limit stays 4 MiB ([ADR-0031](../adr/0031-saved-queries-as-sql-files.md) D7)
- **Implementation:** Partial. A is done (`jykre::script_outline::soft_wrap`,
  measured below); C, the upstream issue, is drafted and not filed. On the
  reference machine (2026-11-08) a 4 MiB file opens in one frame of 54 ms,
  one just under 256 KiB, still wrapped, in 185 ms, and a click into a
  4 MiB text stalls 97–131 ms inside the editor component
  ([measurements](../standards/performance.md#on-the-reference-machine-2026-11-08));
  confirming the limit in ADR-0031 is the maintainer's. The editor work it
  builds on is in [Performance](../standards/performance.md#the-editor-with-a-large-sql-file)
- **Date:** 2026-11-08

## Where things stand

ADR-0031 (D7) keeps a 4 MiB limit for a file opened into a tab only if the
editor meets the frame budget at that size. Two costs stood in the way:

1. **Ours, on every edit and notification.** The workspace copied the whole
   text on every editor notification (scroll, cursor move, blink) and, on a
   change, split the script, counted its lines and scanned its parameters on
   the UI thread. That is gone: edits keep the text in step, the outline is
   read off the UI thread and applied only while it is current, and the
   highlighter relexes in place. Typing in a 4 MiB file now costs our code
   about 0.5 ms a key on the UI thread (1.1 ms at most), down from about
   140 ms; the longest stall while typing fell from 3.7 s to the heartbeat's
   floor (measurements in [Performance](../standards/performance.md#the-editor-with-a-large-sql-file)).
2. **The editor component's, once, when a file opens.** `gpui-kit` 0.9.0
   (`gpui-base`) soft-wraps the editor by default, and on the first layout
   it measures every line of the document with the text shaper
   (`TextWrapper::update_wrapped` → `measured_wrap`, shaping through
   `cosmic_text`). That is about 1.2 ms per KiB on the Linux machine measured
   (1.4 ms on the reference machine): 81 ms at 64 KiB, 316 ms at 256 KiB,
   1.2 s at 1 MiB and 5.1 s at 4 MiB, in one frame. Our code's share of
   opening is now 0.4–13 ms. We do not patch or fork the component.

With soft wrap off (`EditorState::set_soft_wrap(false)`), measured once as
an experiment and not kept, the same files open with a longest frame of
22 ms at 1 MiB and 62 ms at 4 MiB (Linux, release). What is left at 4 MiB is
reading the highlighter's tokens of the whole text (about 35 ms) and
building the rope, once.

## Options

| Option | What the user sees | Cost and risk |
| --- | --- | --- |
| A. Turn soft wrap off for a file above a threshold (for example 256 KiB) | Long lines of a large file scroll sideways instead of wrapping; smaller files are unchanged | A few lines in the workspace; no dependency change. Typing stays as measured. A window resize does not re-wrap the large file either |
| B. Lower the limit and keep soft wrap | Files above the limit are refused with the size and the limit | No size within reach meets the frame budget: 64 KiB already takes 81–131 ms. Keeping the opening stall under the 300 ms progress threshold ([User experience](../reference/ux.md#always-visible-states)) needs about 200 KiB on the reference machine |
| C. Ask upstream for lazy or background wrapping (wrap only rows on screen, or measure lines off the UI thread) | Nothing changes until a release has it | Needs an issue or pull request to `gpui-kit`, then a dependency upgrade, which the maintainer approves ([Stack and dependencies](../standards/stack-and-dependencies.md)) |
| D. Keep the limit and the stall, show a notice before opening a large file | The window freezes for seconds with a warning first | Does not meet D7 |

## Recommendation

**A, with C in parallel; keep the 4 MiB limit.** With soft wrap off above
256 KiB, every size up to 4 MiB measured opens in one frame of at most
62 ms and types within the budgets. That needs confirming on the reference
machine before ADR-0031 records it. If the maintainer prefers soft wrap for
every file, the number for D7 is **192 KiB**: at 1.4 ms per KiB it opens in
about 270 ms, under the 300 ms progress threshold. It still misses the frame
budget, and D7 would record that as a known deviation.

## To decide

- A, B or another option, and the threshold or the limit.
- Whether to open the upstream issue (C).
- Measuring the chosen option on the reference machine (Windows), since the
  numbers above come from Linux with software rendering.
