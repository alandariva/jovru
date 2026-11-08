//! What a chain step receives (specification, section 9.3.5): each input of
//! the step's version from the chain's form, an earlier step's output or a
//! constant, checked as a form's value would be.
use super::definition::{ChainStep, JobDefinition, Source};
use super::outputs::Output;
use crate::parameters::ParameterProblem;
use crate::reports::{ParameterSpec, params};
use std::collections::HashMap;

/// A step's input whose mapped value did not pass the form's checks (cause
/// `Input`). Names only, never the value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputProblem {
    pub input: String,
    pub problem: ParameterProblem,
}

/// A step's inputs, checked.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct StepInputs {
    /// What a form would hold for each input, for a SQL Job's binding
    /// (NULL is the empty text, as an empty field).
    pub typed: HashMap<String, String>,
    /// Each input's value in declaration order, `None` for NULL, for a
    /// program's `hello`.
    pub values: Vec<(String, Option<String>)>,
}

impl std::fmt::Debug for StepInputs {
    /// Never the values.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StepInputs")
            .field(
                "inputs",
                &self.values.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// The values a job's own form gives, checked: `None` for NULL. Every
/// problem is reported, once per input, so each field can show its own.
pub fn form_values(
    specs: &[ParameterSpec],
    typed: &HashMap<String, String>,
) -> Result<Vec<(String, Option<String>)>, Vec<InputProblem>> {
    let mut values = Vec::new();
    let mut problems = Vec::new();
    for spec in specs {
        let text = typed.get(&spec.name).map_or("", String::as_str);
        match params::value(spec, text) {
            Ok(value) => values.push((spec.name.clone(), value)),
            Err(problem) => problems.push(InputProblem {
                input: spec.name.clone(),
                problem,
            }),
        }
    }
    if problems.is_empty() {
        Ok(values)
    } else {
        Err(problems)
    }
}

/// The inputs of `step`, whose version is `target`, from the chain's checked
/// values and the outputs of the steps that ran before it. An input left
/// unbound takes its default, or NULL when optional.
pub fn step_inputs(
    step: &ChainStep,
    target: &JobDefinition,
    chain_values: &[(String, Option<String>)],
    earlier: &HashMap<String, Vec<Output>>,
) -> Result<StepInputs, InputProblem> {
    let mut inputs = StepInputs::default();
    for spec in target.inputs() {
        let mapped: Option<String> = match step.source(&spec.name) {
            None => spec.default.clone(),
            Some(Source::Value(constant)) => Some(constant.clone()),
            Some(Source::Input(name)) => chain_values
                .iter()
                .find(|(input, _)| input == name)
                .and_then(|(_, value)| value.clone()),
            Some(Source::Step { step, output }) => earlier
                .get(step)
                .and_then(|outputs| outputs.iter().find(|candidate| &candidate.name == output))
                .and_then(|output| output.value.clone()),
        };
        let typed = mapped.unwrap_or_default();
        let value = params::value(spec, &typed).map_err(|problem| InputProblem {
            input: spec.name.clone(),
            problem,
        })?;
        inputs.typed.insert(spec.name.clone(), typed);
        inputs.values.push((spec.name.clone(), value));
    }
    Ok(inputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::definition::{JobBody, OutputSpec, Protocol};
    use jykre_core::ParameterKind;
    use jykre_core::automation::JobId;

    fn program() -> JobDefinition {
        let mut job = JobDefinition::new_process("p");
        if let JobBody::Process(body) = &mut job.body {
            body.protocol = Protocol::Jjp1;
            let mut count = ParameterSpec::new("count");
            count.kind = ParameterKind::Number;
            let mut note = ParameterSpec::new("note");
            note.required = false;
            let mut format = ParameterSpec::new("format");
            format.default = Some("csv".into());
            body.inputs = vec![count, note, format];
            body.outputs = vec![OutputSpec {
                name: "file".into(),
                kind: ParameterKind::Text,
            }];
        }
        job
    }

    #[test]
    fn inputs_come_from_outputs_chain_inputs_constants_and_defaults() {
        let mut step = ChainStep::new("export", JobId::new(), 1);
        step.inputs.push((
            "count".into(),
            Source::Step {
                step: "lookup".into(),
                output: "rows".into(),
            },
        ));
        step.inputs
            .push(("note".into(), Source::Input("why".into())));
        let earlier = HashMap::from([(
            "lookup".to_owned(),
            vec![Output {
                name: "rows".into(),
                kind: ParameterKind::Number,
                value: Some(" 42 ".into()),
            }],
        )]);
        let chain_values = vec![("why".to_owned(), None)];
        let inputs = step_inputs(&step, &program(), &chain_values, &earlier).unwrap();
        assert_eq!(
            inputs.values,
            vec![
                ("count".to_owned(), Some("42".to_owned())),
                ("note".to_owned(), None),
                ("format".to_owned(), Some("csv".to_owned())),
            ]
        );
        assert_eq!(inputs.typed["note"], "");
        assert!(!format!("{inputs:?}").contains("42"));
    }

    #[test]
    fn a_null_or_malformed_mapped_value_fails_like_a_form() {
        let mut step = ChainStep::new("export", JobId::new(), 1);
        step.inputs.push((
            "count".into(),
            Source::Step {
                step: "lookup".into(),
                output: "rows".into(),
            },
        ));
        let null = HashMap::from([(
            "lookup".to_owned(),
            vec![Output {
                name: "rows".into(),
                kind: ParameterKind::Number,
                value: None,
            }],
        )]);
        assert_eq!(
            step_inputs(&step, &program(), &[], &null),
            Err(InputProblem {
                input: "count".into(),
                problem: ParameterProblem::Missing
            })
        );
        step.inputs = vec![("count".into(), Source::Value("many".into()))];
        assert_eq!(
            step_inputs(&step, &program(), &[], &HashMap::new()),
            Err(InputProblem {
                input: "count".into(),
                problem: ParameterProblem::Invalid
            })
        );
    }

    #[test]
    fn form_values_report_every_problem() {
        let job = program();
        let typed = HashMap::from([("count".to_owned(), "x".to_owned())]);
        let problems = form_values(job.inputs(), &typed).unwrap_err();
        assert_eq!(
            problems,
            vec![
                InputProblem {
                    input: "count".into(),
                    problem: ParameterProblem::Invalid
                },
                // The form starts with the default; an emptied field is missing.
                InputProblem {
                    input: "format".into(),
                    problem: ParameterProblem::Missing
                }
            ]
        );
        let typed = HashMap::from([
            ("count".to_owned(), "3".to_owned()),
            ("format".to_owned(), "tsv".to_owned()),
        ]);
        assert_eq!(
            form_values(job.inputs(), &typed).unwrap(),
            vec![
                ("count".to_owned(), Some("3".to_owned())),
                ("note".to_owned(), None),
                ("format".to_owned(), Some("tsv".to_owned())),
            ]
        );
    }
}
