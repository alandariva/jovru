//! `jjp/1` messages (section 9.2.2): what a line holds, whether its params
//! are valid, and the responses. Pure; the connection decides what to do.
use super::{
    JjpProgress, MAX_FAIL_CODE, MAX_FAIL_MESSAGE_BYTES, MAX_MESSAGE_BYTES, MAX_OUTPUTS_BYTES,
    MAX_STAGE_CHARS, MAX_VALUE_BYTES, NamedValues, PROTOCOL, RunShared,
};
use crate::automation::definition::OutputSpec;
use jykre_core::automation::parameter_kind_str;
use jykre_core::is_parameter_value;
use serde_json::{Map, Value, json};
use std::time::Instant;

/// A JSON-RPC 2.0 request (with `id`) or notification (without).
#[derive(Debug, PartialEq)]
pub(super) struct Incoming {
    pub id: Option<Value>,
    pub method: String,
    pub params: Value,
}

/// Reads a parsed line as a request or notification. A batch, a response,
/// or anything else that is not a valid request gives the `id` to answer
/// `-32600` with (`null` when it has none).
pub(super) fn classify(value: Value) -> Result<Incoming, Value> {
    // Batches are refused (section 9.2.1), like any other non-object.
    let Value::Object(mut object) = value else {
        return Err(Value::Null);
    };
    let id = match object.remove("id") {
        None => None,
        Some(id @ (Value::String(_) | Value::Number(_) | Value::Null)) => Some(id),
        Some(_) => return Err(Value::Null),
    };
    let answer = id.clone().unwrap_or(Value::Null);
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(answer);
    }
    let Some(Value::String(method)) = object.remove("method") else {
        return Err(answer);
    };
    let params = object.remove("params").unwrap_or(Value::Null);
    if !matches!(params, Value::Object(_) | Value::Array(_) | Value::Null) {
        return Err(answer);
    }
    Ok(Incoming { id, method, params })
}

/// The token of a `hello`, when its params are
/// `{"token": "…", "protocol": "jjp/1"}`.
pub(super) fn hello_token(params: &Value) -> Result<&str, &'static str> {
    let protocol = params.get("protocol").and_then(Value::as_str);
    let token = params.get("token").and_then(Value::as_str);
    match (protocol, token) {
        (Some(PROTOCOL), Some(token)) => Ok(token),
        (Some(_), Some(_)) => Err("Unsupported protocol; this host speaks jjp/1"),
        _ => Err("hello takes {\"token\": \"…\", \"protocol\": \"jjp/1\"}"),
    }
}

/// `hello`'s result: the run's ID, its inputs, the time left and the line
/// cap.
pub(super) fn hello_result(run: &RunShared, now: Instant) -> Value {
    let inputs: Map<String, Value> = run
        .inputs
        .iter()
        .map(|(name, value)| {
            (
                name.to_owned(),
                value.map_or(Value::Null, |value| Value::String(value.to_owned())),
            )
        })
        .collect();
    let left = run.deadline.saturating_duration_since(now).as_millis();
    json!({
        "run_id": run.run.to_string(),
        "inputs": inputs,
        "deadline_ms": u64::try_from(left).unwrap_or(u64::MAX),
        "max_message_bytes": MAX_MESSAGE_BYTES,
    })
}

/// A `log` notification's level and message.
pub(super) fn log_params(params: &Value) -> Option<(&str, &str)> {
    let level = params.get("level").and_then(Value::as_str)?;
    let message = params.get("message").and_then(Value::as_str)?;
    matches!(level, "info" | "warn" | "error").then_some((level, message))
}

/// A `progress` notification: a percent from 0 to 100 and an optional stage
/// of at most [`MAX_STAGE_CHARS`] characters.
pub(super) fn progress_params(params: &Value) -> Option<JjpProgress> {
    let percent = params.get("percent").and_then(Value::as_f64)?;
    if !(0.0..=100.0).contains(&percent) {
        return None;
    }
    let stage = match params.get("stage") {
        None | Some(Value::Null) => None,
        Some(Value::String(stage)) if stage.chars().count() <= MAX_STAGE_CHARS => {
            Some(stage.clone())
        }
        Some(_) => return None,
    };
    Some(JjpProgress { percent, stage })
}

/// Why a `complete` was refused; the program may send a correct one. The
/// message names the output, never its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum OutputProblem {
    /// `params.outputs` is not an object.
    NotAnObject,
    Undeclared(String),
    Missing(String),
    /// Neither a string nor `null` (numbers are refused: nothing passes
    /// through floating point).
    NotText(String),
    /// Not a value of the declared kind.
    Kind(String),
    /// Longer than [`MAX_VALUE_BYTES`].
    TooLong(String),
    /// Together longer than [`MAX_OUTPUTS_BYTES`].
    TooLarge,
}

impl OutputProblem {
    pub(super) fn message(&self, outputs: &[OutputSpec]) -> String {
        match self {
            OutputProblem::NotAnObject => "complete takes {\"outputs\": {…}}".to_owned(),
            OutputProblem::Undeclared(name) => format!("Output '{name}' is not declared"),
            OutputProblem::Missing(name) => format!("Output '{name}' is missing"),
            OutputProblem::NotText(name) => {
                format!("Output '{name}' must be a string or null")
            }
            OutputProblem::Kind(name) => {
                let kind = outputs
                    .iter()
                    .find(|output| &output.name == name)
                    .map_or("text", |output| parameter_kind_str(output.kind));
                format!("Output '{name}' is not a valid {kind}")
            }
            OutputProblem::TooLong(name) => {
                format!("Output '{name}' is longer than {MAX_VALUE_BYTES} bytes")
            }
            OutputProblem::TooLarge => {
                format!("The outputs together are longer than {MAX_OUTPUTS_BYTES} bytes")
            }
        }
    }

    pub(super) fn data(&self) -> Value {
        let (output, problem) = match self {
            OutputProblem::NotAnObject => (None, "not_an_object"),
            OutputProblem::Undeclared(name) => (Some(name), "undeclared"),
            OutputProblem::Missing(name) => (Some(name), "missing"),
            OutputProblem::NotText(name) => (Some(name), "not_text"),
            OutputProblem::Kind(name) => (Some(name), "kind"),
            OutputProblem::TooLong(name) => (Some(name), "too_long"),
            OutputProblem::TooLarge => (None, "too_large"),
        };
        json!({"output": output, "problem": problem})
    }
}

/// A name the program made up, cut for the error message.
fn shown(name: &str) -> String {
    name.chars().take(64).collect()
}

/// Checks a `complete` against the declaration (section 9.3.2): every
/// declared output present (`null` is NULL), nothing undeclared, each text
/// a value of its kind (`Text` takes any) of at most [`MAX_VALUE_BYTES`],
/// all together at most [`MAX_OUTPUTS_BYTES`]. Absent `outputs` count as
/// an empty object.
pub(super) fn check_outputs(
    declared: &[OutputSpec],
    params: &Value,
) -> Result<NamedValues, OutputProblem> {
    let empty = Map::new();
    let given = match params.get("outputs") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(given)) => given,
        Some(_) => return Err(OutputProblem::NotAnObject),
    };
    if let Some(name) = given
        .keys()
        .find(|name| !declared.iter().any(|output| &output.name == *name))
    {
        return Err(OutputProblem::Undeclared(shown(name)));
    }
    let mut values = Vec::with_capacity(declared.len());
    let mut total = 0_usize;
    for output in declared {
        let value = match given.get(&output.name) {
            None => return Err(OutputProblem::Missing(output.name.clone())),
            Some(Value::Null) => None,
            Some(Value::String(text)) => {
                if text.len() > MAX_VALUE_BYTES {
                    return Err(OutputProblem::TooLong(output.name.clone()));
                }
                if !is_parameter_value(output.kind, text) {
                    return Err(OutputProblem::Kind(output.name.clone()));
                }
                total += text.len();
                Some(text.clone())
            }
            Some(_) => return Err(OutputProblem::NotText(output.name.clone())),
        };
        values.push((output.name.clone(), value));
    }
    if total > MAX_OUTPUTS_BYTES {
        return Err(OutputProblem::TooLarge);
    }
    Ok(NamedValues::new(values))
}

/// A valid `fail`: its code, and its message cut to
/// [`MAX_FAIL_MESSAGE_BYTES`] on a character boundary.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Failure {
    pub code: String,
    pub message: String,
    pub cut: bool,
}

/// `fail`'s params: a code of 1 to [`MAX_FAIL_CODE`] characters of
/// `A-Z a-z 0-9 _ . -`, and an optional message.
pub(super) fn fail_params(params: &Value) -> Result<Failure, &'static str> {
    const CODE_RULE: &str = "fail.code must be 1 to 64 characters of A-Z a-z 0-9 _ . -";
    let code = params
        .get("code")
        .and_then(Value::as_str)
        .ok_or(CODE_RULE)?;
    if code.is_empty()
        || code.len() > MAX_FAIL_CODE
        || !code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    {
        return Err(CODE_RULE);
    }
    let message = match params.get("message") {
        None | Some(Value::Null) => "",
        Some(Value::String(message)) => message,
        Some(_) => return Err("fail.message must be a string"),
    };
    let mut end = message.len().min(MAX_FAIL_MESSAGE_BYTES);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    Ok(Failure {
        code: code.to_owned(),
        message: message.get(..end).unwrap_or_default().to_owned(),
        cut: end < message.len(),
    })
}

/// One line: the value and a newline.
fn line(value: &Value) -> Vec<u8> {
    // Serializing a `Value` cannot fail: its keys are strings.
    let mut bytes = serde_json::to_vec(value).unwrap_or_default();
    bytes.push(b'\n');
    bytes
}

pub(super) fn result(id: &Value, result: Value) -> Vec<u8> {
    line(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

pub(super) fn error(id: &Value, code: i64, message: &str, data: Option<Value>) -> Vec<u8> {
    let mut error = json!({"code": code, "message": message});
    if let (Some(data), Value::Object(object)) = (data, &mut error) {
        object.insert("data".to_owned(), data);
    }
    line(&json!({"jsonrpc": "2.0", "id": id, "error": error}))
}

pub(super) fn cancel() -> Vec<u8> {
    line(&json!({"jsonrpc": "2.0", "method": "cancel", "params": {}}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jykre_core::ParameterKind;

    fn outputs() -> Vec<OutputSpec> {
        vec![
            OutputSpec {
                name: "rows".into(),
                kind: ParameterKind::Number,
            },
            OutputSpec {
                name: "note".into(),
                kind: ParameterKind::Text,
            },
        ]
    }

    #[test]
    fn requests_notifications_and_invalid_messages_are_told_apart() {
        let request =
            classify(json!({"jsonrpc": "2.0", "id": 7, "method": "complete", "params": {}}));
        assert_eq!(
            request,
            Ok(Incoming {
                id: Some(json!(7)),
                method: "complete".into(),
                params: json!({}),
            })
        );
        let notification = classify(json!({"jsonrpc": "2.0", "method": "log"})).unwrap();
        assert_eq!(notification.id, None);
        assert_eq!(notification.params, Value::Null);
        assert_eq!(
            classify(json!([{"jsonrpc": "2.0", "method": "log"}])),
            Err(Value::Null)
        );
        assert_eq!(
            classify(json!({"jsonrpc": "1.0", "id": "a", "method": "x"})),
            Err(json!("a"))
        );
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "id": 1, "result": {}})),
            Err(json!(1))
        );
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "id": {}, "method": "x"})),
            Err(Value::Null)
        );
        assert_eq!(
            classify(json!({"jsonrpc": "2.0", "id": 2, "method": "x", "params": 3})),
            Err(json!(2))
        );
    }

    #[test]
    fn complete_must_carry_every_declared_output_and_nothing_else() {
        let declared = outputs();
        let good = check_outputs(
            &declared,
            &json!({"outputs": {"rows": "12.50", "note": null}}),
        )
        .unwrap();
        assert_eq!(good.get("rows"), Some(Some("12.50")));
        assert_eq!(good.get("note"), Some(None));
        assert!(!format!("{good:?}").contains("12.50"));

        let cases = [
            (
                json!({"outputs": {"rows": "1"}}),
                OutputProblem::Missing("note".into()),
            ),
            (
                json!({"outputs": {"rows": "1", "note": "x", "extra": "y"}}),
                OutputProblem::Undeclared("extra".into()),
            ),
            (
                json!({"outputs": {"rows": 12, "note": "x"}}),
                OutputProblem::NotText("rows".into()),
            ),
            (
                json!({"outputs": {"rows": "twelve", "note": "x"}}),
                OutputProblem::Kind("rows".into()),
            ),
            (json!({"outputs": []}), OutputProblem::NotAnObject),
        ];
        for (params, problem) in cases {
            assert_eq!(check_outputs(&declared, &params), Err(problem.clone()));
            let message = problem.message(&declared);
            assert!(
                !message.contains("twelve") && !message.contains("\"x\""),
                "{message}"
            );
        }
        assert_eq!(
            OutputProblem::Kind("rows".into()).message(&declared),
            "Output 'rows' is not a valid number"
        );

        let long = "a".repeat(MAX_VALUE_BYTES + 1);
        assert_eq!(
            check_outputs(&declared, &json!({"outputs": {"rows": "1", "note": long}})),
            Err(OutputProblem::TooLong("note".into()))
        );
        let many: Vec<OutputSpec> = (0..5)
            .map(|index| OutputSpec {
                name: format!("o{index}"),
                kind: ParameterKind::Text,
            })
            .collect();
        let value = "b".repeat(MAX_VALUE_BYTES);
        let all: Map<String, Value> = many
            .iter()
            .map(|output| (output.name.clone(), json!(value)))
            .collect();
        assert_eq!(
            check_outputs(&many, &json!({"outputs": all})),
            Err(OutputProblem::TooLarge)
        );
        assert_eq!(check_outputs(&[], &json!({})), Ok(NamedValues::default()));
    }

    #[test]
    fn fail_codes_are_checked_and_messages_cut_on_a_character() {
        let failure = fail_params(&json!({"code": "E_NO.DATA-1", "message": "why"})).unwrap();
        assert_eq!(failure.code, "E_NO.DATA-1");
        assert_eq!(failure.message, "why");
        assert!(!failure.cut);
        for code in [
            json!(""),
            json!("has space"),
            json!("ç"),
            json!("x".repeat(65)),
            json!(5),
        ] {
            assert!(fail_params(&json!({"code": code})).is_err());
        }
        let long = "é".repeat(MAX_FAIL_MESSAGE_BYTES);
        let failure = fail_params(&json!({"code": "E", "message": long})).unwrap();
        assert!(failure.cut);
        assert_eq!(failure.message.len(), MAX_FAIL_MESSAGE_BYTES);
        assert!(failure.message.chars().all(|c| c == 'é'));
    }

    #[test]
    fn progress_and_log_params_are_checked() {
        assert_eq!(
            progress_params(&json!({"percent": 42.5, "stage": "export"})),
            Some(JjpProgress {
                percent: 42.5,
                stage: Some("export".into())
            })
        );
        assert!(progress_params(&json!({"percent": 101})).is_none());
        assert!(progress_params(&json!({"percent": "50"})).is_none());
        assert!(progress_params(&json!({"percent": 5, "stage": "s".repeat(201)})).is_none());
        assert_eq!(
            log_params(&json!({"level": "warn", "message": "m"})),
            Some(("warn", "m"))
        );
        assert_eq!(log_params(&json!({"level": "debug", "message": "m"})), None);
        assert_eq!(
            hello_token(&json!({"token": "t", "protocol": "jjp/1"})),
            Ok("t")
        );
        assert!(hello_token(&json!({"token": "t", "protocol": "jjp/2"})).is_err());
        assert!(hello_token(&json!({"protocol": "jjp/1"})).is_err());
    }
}
