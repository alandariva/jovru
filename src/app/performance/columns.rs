//! Finding a column (A15) in a result of 10,000 columns: the production
//! column search, opened as `Ctrl Shift F` does, its text typed a character
//! at a time (each change finds the matches again), then matches chosen,
//! one of them hidden by A14, so the grid shows it and pans to it.
use super::*;
use jykre::performance::budget::{OTHER_FRAME_P95, WITHIN_FRAME};
use jykre::performance::data;
use jykre::probe::Samples;

const COLUMNS: usize = 10_000;
const ROWS: usize = 50;
/// What is typed, ignoring accents as the search does.
const TEXTS: [&str; 3] = ["situacao_9", "preco_42", "descricao_77"];
const BETWEEN_KEYS: Duration = Duration::from_millis(120);
/// A column hidden before it is searched for.
const HIDDEN: usize = 7_773;

/// A match chosen leaves no column hidden that it should have shown.
const HIDDEN_SHOWN: Budget = Budget {
    limit: 0.0,
    text: "0 (the hidden match is shown)",
    source: "0.3.0 A15: a match hidden by A14 is shown when chosen",
};

pub(super) async fn run(
    run: &Run,
    cx: &mut AsyncWindowContext,
    section: &mut Section,
) -> Result<(), Closed> {
    run.status(
        cx,
        phase_status(PathId::Columns, "preparing", "10,000 columns"),
    )?;
    let data = cx
        .background_executor()
        .spawn(async { data::named_fixture(ROWS, COLUMNS) })
        .await;
    let grid = run.update(cx, |view, _, cx| {
        let grid = cx.new(|cx| ResultGrid::new(Arc::default(), false, cx));
        grid.update(cx, |grid, cx| {
            grid.set_page(Arc::new(data), false, 0, None, cx)
        });
        view.stage = Stage::Grid(grid.clone());
        cx.notify();
        grid
    })?;
    run.wait(cx, Duration::from_millis(1500)).await;

    // Typing: each keystroke finds the matches again.
    let clock = run
        .begin(cx, phase_status(PathId::Columns, "typing", "starting"))
        .await?;
    let mut keys = Samples::default();
    let mut found = Vec::new();
    for text in TEXTS {
        run.update(cx, |_, window, cx| {
            grid.update(cx, |grid, cx| grid.benchmark_find_open(window, cx));
        })?;
        for (index, key) in text.char_indices() {
            let key = key.to_string();
            let ((), took) = run.timed(cx, |view, window, cx| {
                grid.update(cx, |grid, cx| {
                    grid.benchmark_find_type(&key, index == 0, window, cx)
                });
                view.status = phase_status(PathId::Columns, "typing", &text[..index + key.len()]);
                cx.notify();
            })?;
            keys.push(took);
            run.wait(cx, BETWEEN_KEYS).await;
        }
        let matches = run.update(cx, |_, _, cx| grid.read(cx).benchmark_find_matches())?;
        found.push(json!({"text": text, "matches": matches}));
    }
    let numbers = run.end(cx, clock).await;
    let keys = keys.take_summary();
    section.phases.push(numbers.to_json(
        "typing",
        json!({"columns": COLUMNS, "rows": ROWS, "found": found, "keystroke_ui_thread": summary_json(keys)}),
    ));
    section.metrics.push(Metric::maybe(
        "A keystroke's UI-thread time (matches found again), p95",
        keys.map(|keys| ms(keys.p95)),
        "ms",
        Some(WITHIN_FRAME),
        "nothing was typed",
    ));
    section.metrics.push(Metric::maybe(
        "A keystroke's UI-thread time, longest",
        keys.map(|keys| ms(keys.max)),
        "ms",
        None,
        "nothing was typed",
    ));
    numbers.frame_metrics(section, "typing", OTHER_FRAME_P95, None, None);

    // Choosing: matches across the width, then one A14 hid.
    let clock = run
        .begin(cx, phase_status(PathId::Columns, "choose", "starting"))
        .await?;
    let mut chosen = Samples::default();
    let mut missed = 0usize;
    run.update(cx, |_, window, cx| {
        grid.update(cx, |grid, cx| {
            grid.benchmark_find_open(window, cx);
            grid.benchmark_find_type("situacao_9", true, window, cx);
        });
    })?;
    for step in 0..20 {
        // Columns 9006, 9056, … 9956: each far from the last.
        let column = 9_006 + (step * 50) % 1_000;
        let (done, took) = run.timed(cx, |view, window, cx| {
            view.status = phase_status(PathId::Columns, "choose", &data::column_name(column));
            cx.notify();
            grid.update(cx, |grid, cx| {
                grid.benchmark_find_choose(Some(column), window, cx)
            })
        })?;
        if done {
            chosen.push(took);
        } else {
            missed += 1;
        }
        run.wait(cx, Duration::from_millis(200)).await;
        // The field keeps its text; going back to it as a click would.
        run.update(cx, |_, window, cx| {
            grid.update(cx, |grid, cx| grid.benchmark_find_open(window, cx));
        })?;
    }
    run.update(cx, |_, window, cx| {
        grid.update(cx, |grid, cx| {
            grid.benchmark_hide(HIDDEN, cx);
            grid.benchmark_find_open(window, cx);
            grid.benchmark_find_type(&data::column_name(HIDDEN), true, window, cx);
        });
    })?;
    run.wait(cx, Duration::from_millis(200)).await;
    let (hidden_chosen, took) = run.timed(cx, |view, window, cx| {
        view.status = phase_status(PathId::Columns, "choose", "a hidden match");
        cx.notify();
        grid.update(cx, |grid, cx| {
            grid.benchmark_find_choose(Some(HIDDEN), window, cx)
        })
    })?;
    if hidden_chosen {
        chosen.push(took);
    } else {
        missed += 1;
    }
    run.wait(cx, Duration::from_millis(500)).await;
    let still_hidden = run.update(cx, |_, _, cx| grid.read(cx).benchmark_hidden(HIDDEN))?;
    let numbers = run.end(cx, clock).await;
    let chosen = chosen.take_summary();
    section.phases.push(numbers.to_json(
        "choose",
        json!({"choices": 21, "not_found": missed, "hidden_column_shown": !still_hidden, "choose_ui_thread": summary_json(chosen)}),
    ));
    section.metrics.push(Metric::maybe(
        "Choosing a match (scroll and select), p95",
        chosen.map(|chosen| ms(chosen.p95)),
        "ms",
        Some(WITHIN_FRAME),
        "no match was chosen",
    ));
    section.metrics.push(Metric::checked(
        "A hidden match chosen and still hidden",
        if hidden_chosen && !still_hidden {
            0.0
        } else {
            1.0
        },
        "columns",
        HIDDEN_SHOWN,
    ));
    numbers.frame_metrics(section, "choosing matches", OTHER_FRAME_P95, None, None);
    section.notes.push(format!(
        "{COLUMNS} columns named by words with accents and a number; typed without them, a character every {} ms.",
        BETWEEN_KEYS.as_millis()
    ));
    Ok(())
}
