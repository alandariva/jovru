//! Go to a line (A9) in a real SQL editor holding a text above 256 KiB:
//! the production `EditorState` with the editor adapter and its **Go to
//! line** prompt, as a query tab builds them. Each jump opens the prompt as
//! `Ctrl G` does, puts the target in its field and goes as `Enter` does.
use super::*;
use jykre::performance::budget::{OTHER_FRAME_P95, WITHIN_FRAME};
use jykre::performance::data;
use jykre::probe::Samples;

/// The text's size: four times the threshold above which it is not
/// wrapped.
const TEXT_BYTES: usize = 1024 * 1024;
const JUMPS: usize = 60;
const BETWEEN_JUMPS: Duration = Duration::from_millis(100);

pub(super) async fn run(
    run: &Run,
    cx: &mut AsyncWindowContext,
    section: &mut Section,
) -> Result<(), Closed> {
    run.status(
        cx,
        phase_status(PathId::Editor, "preparing", "1 MiB of SQL"),
    )?;
    let text = cx
        .background_executor()
        .spawn(async { data::sql_script(TEXT_BYTES) })
        .await;
    let bytes = text.len();
    let clock = run
        .begin(cx, phase_status(PathId::Editor, "open", "1 MiB of SQL"))
        .await?;
    let ((), opened) = run.timed(cx, move |view, window, cx| {
        let wrap = jykre::script_outline::soft_wrap(text.len(), true);
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .default_value(text)
        });
        // Before the first layout, as a query tab does: a long text is
        // never wrapped.
        editor.update(cx, |editor, cx| editor.set_soft_wrap(wrap, window, cx));
        let sql_editor = cx.new(|cx| SqlEditor::new(&editor, cx));
        let prompt = cx.new(|_| GoToLine::new(&sql_editor));
        // The adapter lives as long as the prompt that holds it weakly.
        view.stage = Stage::Editor {
            editor,
            prompt,
            _adapter: sql_editor,
        };
        cx.notify();
    })?;
    run.wait(cx, Duration::from_secs(3)).await;
    let numbers = run.end(cx, clock).await;
    let lines = run.update(cx, |view, _, cx| match &view.stage {
        Stage::Editor { editor, .. } => {
            use gpui_kit::component::RopeExt as _;
            editor.read(cx).text().lines_len()
        }
        _ => 0,
    })?;
    section.phases.push(numbers.to_json(
        "open",
        json!({"bytes": bytes, "lines": lines, "ui_thread_ms": ms(opened)}),
    ));
    section.metrics.push(Metric::info(
        "Opening the 1 MiB text: UI-thread time",
        ms(opened),
        "ms",
    ));
    section.metrics.push(Metric::maybe(
        "Opening the 1 MiB text: longest frame",
        numbers.frames().map(|frames| ms(frames.max)),
        "ms",
        None,
        PROBE_OFF,
    ));

    let targets = data::jump_targets(lines, JUMPS);
    let clock = run
        .begin(cx, phase_status(PathId::Editor, "go_to_line", "starting"))
        .await?;
    let mut jumps = Samples::default();
    let mut past_end = 0;
    for (index, target) in targets.iter().enumerate() {
        if let Some(parsed) = jykre::go_to_line::parse(target)
            && jykre::go_to_line::place(parsed, lines).past_end
        {
            past_end += 1;
        }
        let ((), took) = run.timed(cx, |view, window, cx| {
            if let Stage::Editor { prompt, .. } = &view.stage {
                prompt.update(cx, |prompt, cx| prompt.benchmark_go(target, window, cx));
            }
            view.status = phase_status(
                PathId::Editor,
                "go_to_line",
                &format!("{} of {JUMPS}", index + 1),
            );
            cx.notify();
        })?;
        jumps.push(took);
        run.wait(cx, BETWEEN_JUMPS).await;
    }
    let numbers = run.end(cx, clock).await;
    let jumps = jumps.take_summary();
    section.phases.push(numbers.to_json(
        "go_to_line",
        json!({"jumps": targets.len(), "past_end": past_end, "jump_ui_thread": summary_json(jumps)}),
    ));
    section.metrics.push(Metric::maybe(
        "A jump's UI-thread time, p95",
        jumps.map(|jumps| ms(jumps.p95)),
        "ms",
        Some(WITHIN_FRAME),
        "no jump was made",
    ));
    section.metrics.push(Metric::maybe(
        "A jump's UI-thread time, longest",
        jumps.map(|jumps| ms(jumps.max)),
        "ms",
        None,
        "no jump was made",
    ));
    numbers.frame_metrics(section, "jumping", OTHER_FRAME_P95, None, None);
    section.notes.push(format!(
        "{bytes} bytes in {lines} lines, not wrapped; {} jumps, {past_end} of them past the last line.",
        targets.len()
    ));
    Ok(())
}
