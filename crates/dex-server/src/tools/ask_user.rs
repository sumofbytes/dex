//! The `ask_user` tool: structured questions parked on the active surface
//! (TUI modal, headless numbered prompt, remote client). The call blocks
//! like any tool; the answer comes back as an ordinary tool result.
//!
//! The executor is pure plumbing: it clamps the model-supplied arguments
//! server-side (schema bounds are advisory for some models), sends one
//! [`QuestionRequest`] on the console's question channel, and formats the
//! resolved answers. Parking, journaling, wire broadcast, teardown and the
//! child timeout live in `daemon/questions.rs` — the same split as
//! `policy::enforce_policy` vs `daemon/approvals.rs`.

use serde_json::{Map, Value};
use tokio::sync::mpsc;

use dex_protocol::{Question, QuestionAnswer, QuestionOption};

use crate::protocol::QuestionRequest;
use crate::runtime::cancel::CancellationSource;

use super::audit::audit;
use super::error::ToolError;
use super::meta::Policy;

/// Hard question-batch bound (mirrors the schema `maxItems`).
pub const MAX_QUESTIONS: usize = 4;
/// Hard option-count bound per question (mirrors the schema `maxItems`).
pub const MAX_OPTIONS: usize = 4;
/// A question with fewer options than this cannot be a meaningful picker.
pub const MIN_OPTIONS: usize = 2;
/// One-line clamp for free-text fields (`question`, "Other" answers).
pub const TEXT_MAX_CHARS: usize = 500;
/// Picker tab label bound (mirrors the schema `maxLength`).
const HEADER_MAX_CHARS: usize = 12;
/// Option label bound (mirrors the schema `maxLength`).
const LABEL_MAX_CHARS: usize = 20;

/// Collapse a model-supplied string to one line and clamp it: newlines
/// become spaces, runs of whitespace collapse, and the tail truncates at
/// `max` chars without splitting a UTF-8 scalar.
fn one_line(raw: &str, max: usize) -> String {
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(max).collect()
}

/// Parse and clamp the `ask_user` arguments server-side. Out-of-bounds
/// counts truncate; out-of-range `default` indices drop; a question with
/// fewer than [`MIN_OPTIONS`] options fails the call (nothing meaningful
/// can be clamped to — a picker needs at least two choices).
pub fn parse_questions(args: &Map<String, Value>) -> Result<Vec<Question>, ToolError> {
    let Some(list) = args.get("questions").and_then(Value::as_array) else {
        return Err(ToolError::InvalidArgument(
            "ask_user needs a 'questions' array".into(),
        ));
    };
    if list.is_empty() {
        return Err(ToolError::InvalidArgument(
            "ask_user needs at least one question".into(),
        ));
    }
    let mut out = Vec::with_capacity(list.len().min(MAX_QUESTIONS));
    for (i, entry) in list.iter().take(MAX_QUESTIONS).enumerate() {
        let question = entry
            .get("question")
            .and_then(Value::as_str)
            .map(|q| one_line(q, TEXT_MAX_CHARS))
            .filter(|q| !q.is_empty())
            .ok_or_else(|| {
                ToolError::InvalidArgument(format!("questions[{i}] needs a 'question' string"))
            })?;
        // Tolerant default: the schema requires `header`, but a missing or
        // oversized one must not fail the batch — pickers fall back to the
        // generic label.
        let header = entry
            .get("header")
            .and_then(Value::as_str)
            .map(|h| one_line(h, HEADER_MAX_CHARS))
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "Question".to_string());
        let Some(options) = entry.get("options").and_then(Value::as_array) else {
            return Err(ToolError::InvalidArgument(format!(
                "questions[{i}] needs an 'options' array"
            )));
        };
        if options.len() < MIN_OPTIONS {
            return Err(ToolError::InvalidArgument(format!(
                "questions[{i}] needs at least {MIN_OPTIONS} options (got {})",
                options.len()
            )));
        }
        let options: Vec<QuestionOption> = options
            .iter()
            .take(MAX_OPTIONS)
            .map(|o| QuestionOption {
                label: o
                    .get("label")
                    .and_then(Value::as_str)
                    .map(|l| one_line(l, LABEL_MAX_CHARS))
                    .unwrap_or_default(),
                description: o
                    .get("description")
                    .and_then(Value::as_str)
                    .map(|d| one_line(d, TEXT_MAX_CHARS))
                    .unwrap_or_default(),
            })
            .collect();
        if options.iter().any(|o| o.label.is_empty()) {
            return Err(ToolError::InvalidArgument(format!(
                "questions[{i}] has an option without a 'label' string"
            )));
        }
        // Tolerant `multiSelect`: omitted or non-bool => false.
        let multi_select = entry
            .get("multiSelect")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // `default` indexes into `options`; anything out of range drops
        // (the headless empty-line shortcut then re-prompts).
        let default = entry
            .get("default")
            .and_then(Value::as_u64)
            .map(|d| d as usize)
            .filter(|d| *d < options.len());
        out.push(Question {
            question,
            header,
            options,
            multi_select,
            default,
        });
    }
    Ok(out)
}

/// Format the tool result the model reads back: one line per question,
/// `answers[i]` corresponding to `questions[i]` (missing slots are
/// dismissed). `Choice` labels index into the question's own `options`.
pub fn format_result(questions: &[Question], answers: &[QuestionAnswer]) -> String {
    let mut lines = Vec::with_capacity(questions.len());
    for (i, question) in questions.iter().enumerate() {
        let answer = answers.get(i);
        let body = match answer {
            Some(QuestionAnswer::Choice(idx)) => question
                .options
                .get(*idx)
                .map(|o| format!("{} (user's choice)", o.label))
                .unwrap_or_else(|| "no answer (user dismissed)".to_string()),
            Some(QuestionAnswer::Multi(idxs)) => {
                let labels: Vec<String> = idxs
                    .iter()
                    .filter_map(|idx| question.options.get(*idx))
                    .map(|o| o.label.clone())
                    .collect();
                if labels.is_empty() {
                    "no answer (user dismissed)".to_string()
                } else {
                    labels.join(", ")
                }
            }
            Some(QuestionAnswer::Text(text)) if !text.trim().is_empty() => {
                one_line(text, TEXT_MAX_CHARS)
            }
            _ => "no answer (user dismissed)".to_string(),
        };
        lines.push(format!("{}: {}", question.header, body));
    }
    lines.join("\n")
}

/// The audit answer column, built from the single-source
/// [`QuestionAnswer::as_str`] plus the resolved labels, so audit rows never
/// drift from the wire spellings: `choice <label>`, `multi <l1,l2>`,
/// `text`, `dismiss`.
fn audit_answer(question: &Question, answer: &QuestionAnswer) -> String {
    match answer {
        QuestionAnswer::Choice(idx) => question
            .options
            .get(*idx)
            .map(|o| format!("choice {}", o.label))
            .unwrap_or_else(|| "choice <out of range>".to_string()),
        QuestionAnswer::Multi(idxs) => {
            let labels: Vec<&str> = idxs
                .iter()
                .filter_map(|idx| question.options.get(*idx))
                .map(|o| o.label.as_str())
                .collect();
            if labels.is_empty() {
                "multi <out of range>".to_string()
            } else {
                format!("multi {}", labels.join(","))
            }
        }
        other => other.as_str().to_string(),
    }
}

/// Execute one `ask_user` call: park a question batch on the active
/// surface and await the whole batch's answers. Never prompts for
/// permission (a `Read`-requirement tool); fails closed when no
/// interactive surface exists.
pub async fn tool_ask_user(
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
) -> Result<String, ToolError> {
    let questions = parse_questions(args)?;
    let Some(console) = policy.console.as_ref() else {
        return Err(ToolError::Denied(
            "ask_user needs an interactive surface; none is attached".into(),
        ));
    };
    let Some(sender) = console.questions() else {
        return Err(ToolError::Denied(
            "ask_user needs an interactive surface; no question channel is attached".into(),
        ));
    };
    let (response_tx, mut response_rx) = mpsc::channel::<Vec<QuestionAnswer>>(1);
    // Same child stamping as the approval gate: the daemon parks and
    // labels the request under the child's id.
    let (agent_id, agent) = match console.agent.as_ref() {
        Some((id, label)) => (Some(id.clone()), Some(label.clone())),
        None => (None, None),
    };
    sender
        .send(QuestionRequest {
            questions: questions.clone(),
            agent_id,
            agent,
            response: response_tx,
        })
        .await
        .map_err(|_| ToolError::Denied("ask_user question channel is closed".into()))?;
    let answers = tokio::select! {
        () = crate::runtime::cancel::wait_cancelled(cancel) => {
            return Err(ToolError::Denied(
                "cancelled while awaiting the user's answer".into(),
            ));
        }
        answers = response_rx.recv() => answers,
    };
    let Some(answers) = answers else {
        return Err(ToolError::Denied(
            "ask_user went unanswered; treating as dismissed".into(),
        ));
    };
    // One audit row for the resolved batch, alongside the dispatch row:
    // DEX_AUDIT=1 must show what the user actually picked, not just that
    // the call succeeded.
    let spelling: Vec<String> = questions
        .iter()
        .zip(&answers)
        .map(|(q, a)| audit_answer(q, a))
        .collect();
    let mut audit_args = args.clone();
    audit_args.insert("answer".into(), Value::String(spelling.join("; ")));
    audit("ask_user", &audit_args, "ask");
    Ok(format_result(&questions, &answers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn sample() -> Value {
        json!({"questions": [{
            "question": "Which database?",
            "header": "Database",
            "options": [
                {"label": "postgres", "description": "default"},
                {"label": "sqlite", "description": "embedded"}
            ],
            "default": 0
        }]})
    }

    #[test]
    fn parse_keeps_valid_input() {
        let questions = parse_questions(&args(sample())).unwrap();
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].header, "Database");
        assert_eq!(questions[0].options.len(), 2);
        assert_eq!(questions[0].default, Some(0));
        assert!(!questions[0].multi_select);
    }

    #[test]
    fn parse_clamps_out_of_bounds_counts_and_lengths() {
        // 5 questions → 4; each with 5 options → 4.
        let question = json!({
            "question": "q", "header": "H",
            "options": (0..5).map(|i| json!({"label": format!("o{i}"), "description": ""})).collect::<Vec<_>>()
        });
        let list = (0..5).map(|_| question.clone()).collect::<Vec<_>>();
        let parsed = parse_questions(&args(json!({"questions": list}))).unwrap();
        assert_eq!(parsed.len(), MAX_QUESTIONS);
        assert_eq!(parsed[0].options.len(), MAX_OPTIONS);
        // Header clamps to 12, labels to 20 chars.
        let long = parse_questions(&args(json!({"questions": [{
            "question": "q", "header": "x".repeat(30),
            "options": [{"label": "y".repeat(40), "description": ""}, {"label": "ok", "description": ""}]
        }]})))
        .unwrap();
        assert_eq!(long[0].header.chars().count(), HEADER_MAX_CHARS);
        assert_eq!(long[0].options[0].label.chars().count(), LABEL_MAX_CHARS);
        // Free-text fields collapse to one line and clamp to 500.
        let text = parse_questions(&args(json!({"questions": [{
            "question": "a\n\n b   c", "header": "H",
            "options": [{"label": "a", "description": ""}, {"label": "b", "description": ""}]
        }]})))
        .unwrap();
        assert_eq!(text[0].question, "a b c");
    }

    #[test]
    fn parse_rejects_unusable_shapes() {
        assert!(parse_questions(&args(json!({}))).is_err());
        assert!(parse_questions(&args(json!({"questions": []}))).is_err());
        // One option cannot become a picker.
        assert!(parse_questions(&args(json!({"questions": [{
            "question": "q", "header": "H", "options": [{"label": "a", "description": ""}]
        }]})))
        .is_err());
        // Missing question / label strings fail; `default` out of range drops.
        assert!(
            parse_questions(&args(json!({"questions": [{"header": "H", "options": [
            {"label": "a", "description": ""}, {"label": "b", "description": ""}]}]})))
            .is_err()
        );
        let parsed = parse_questions(&args(json!({"questions": [{
            "question": "q", "header": "H", "default": 7, "multiSelect": "yes",
            "options": [{"label": "a", "description": ""}, {"label": "b", "description": ""}]
        }]})))
        .unwrap();
        assert_eq!(parsed[0].default, None);
        assert!(!parsed[0].multi_select);
    }

    #[test]
    fn format_covers_every_answer_kind() {
        let questions = parse_questions(&args(sample())).unwrap();
        assert_eq!(
            format_result(questions.as_slice(), &[QuestionAnswer::Choice(1)]),
            "Database: sqlite (user's choice)"
        );
        assert_eq!(
            format_result(questions.as_slice(), &[QuestionAnswer::Dismiss]),
            "Database: no answer (user dismissed)"
        );
        assert_eq!(
            format_result(
                questions.as_slice(),
                &[QuestionAnswer::Text("  sqlite, but on a stick\n".into())]
            ),
            "Database: sqlite, but on a stick"
        );
        // Out-of-bounds Choice renders as dismissed, never panics.
        assert_eq!(
            format_result(questions.as_slice(), &[QuestionAnswer::Choice(9)]),
            "Database: no answer (user dismissed)"
        );
        // Missing slots default to dismissed.
        assert_eq!(
            format_result(questions.as_slice(), &[]),
            "Database: no answer (user dismissed)"
        );
    }

    #[test]
    fn format_multi_select_keeps_selection_order() {
        let list = json!({"questions": [{
        "question": "Pick extras", "header": "Extras", "multiSelect": true,
        "options": [
            {"label": "auth", "description": ""}, {"label": "telemetry", "description": ""},
            {"label": "cache", "description": ""}
        ]}]});
        let questions = parse_questions(&args(list)).unwrap();
        let result = format_result(questions.as_slice(), &[QuestionAnswer::Multi(vec![2, 0])]);
        assert_eq!(result, "Extras: cache, auth");
    }

    #[tokio::test]
    async fn tool_parks_and_formats_the_answer() {
        use crate::protocol::{PermissionMode, QuestionRequest};
        use crate::runtime::console::Console;
        use crate::tools::meta::Policy;

        let questions = parse_questions(&args(sample())).unwrap();
        let (question_tx, mut question_rx) = tokio::sync::mpsc::channel::<QuestionRequest>(1);
        let (sink_tx, _sink_rx) = tokio::sync::mpsc::channel(16);
        let (approval_tx, _approval_rx) = tokio::sync::mpsc::channel(16);
        let console = Console::new(sink_tx, approval_tx).with_questions(Some(question_tx));
        let mut policy = Policy::turn(PermissionMode::Trusted, &console);

        // Park → answer → formatted tool result, end to end through the
        // executor's channel contract.
        let cancel = crate::runtime::console::CancellationToken::new();
        let sample_args = args(sample());
        let executor = tool_ask_user(&sample_args, &cancel, &policy);
        let resolver = tokio::spawn(async move {
            let request = question_rx.recv().await.expect("request parked");
            assert_eq!(request.questions, questions);
            assert_eq!(request.agent_id, None);
            let _ = request
                .response
                .send(vec![dex_protocol::QuestionAnswer::Choice(1)])
                .await;
        });
        let result = executor.await.expect("answered");
        resolver.await.unwrap();
        assert_eq!(result, "Database: sqlite (user's choice)");

        // No question channel: fail closed with a clear denial.
        let bare = Console::none();
        policy.console = Some(bare);
        let err = tool_ask_user(
            &args(sample()),
            &crate::runtime::console::CancellationToken::new(),
            &policy,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("interactive surface"), "{err}");
    }

    #[test]
    fn proptest_parse_never_panics_and_always_bounded() {
        use proptest::prelude::*;

        // Property: for ANY model-supplied shape the executor clamps
        // server-side — parsing never panics, and every parsed batch
        // respects the hard bounds (`MAX_QUESTIONS`, `MAX_OPTIONS`,
        // `MIN_OPTIONS`, header length, in-range `default`).
        fn opt_string(max: usize) -> impl proptest::strategy::Strategy<Value = Value> {
            use proptest::prelude::*;
            proptest::option::of(proptest::collection::vec(any::<char>(), 0..max)).prop_map(|opt| {
                opt.map(|chars| json!(chars.into_iter().collect::<String>()))
                    .unwrap_or(Value::Null)
            })
        }

        fn question_strategy() -> impl proptest::strategy::Strategy<Value = Value> {
            use proptest::prelude::*;
            (
                opt_string(700),
                opt_string(40),
                proptest::prop_oneof![Just(json!("junk")), Just(json!(true)), Just(Value::Null),],
                proptest::option::of(0u64..20),
                0usize..8,
                opt_string(60),
            )
                .prop_map(
                    |(question, header, multi_select, default, n_options, first_label)| {
                        json!({
                            "question": question,
                            "header": header,
                            "multiSelect": multi_select,
                            "default": default,
                            "options": (0..n_options)
                                .map(|i| if i == 0 {
                                    json!({"label": first_label, "description": "d"})
                                } else {
                                    json!({"label": format!("o{i}"), "description": "d"})
                                })
                                .collect::<Vec<_>>()
                        })
                    },
                )
        }

        proptest!(|(n_questions in 0usize..8, question in question_strategy())| {
            let list: Vec<Value> = (0..n_questions)
                .map(|i| {
                    if i == 0 {
                        question.clone()
                    } else {
                        // Later slots exercise the too-few-options rejection.
                        json!({"question": "x", "header": "H", "options": []})
                    }
                })
                .collect();
            if let Ok(parsed) = parse_questions(&args(json!({"questions": list}))) {
                prop_assert!(parsed.len() <= MAX_QUESTIONS);
                for q in &parsed {
                    prop_assert!((MIN_OPTIONS..=MAX_OPTIONS).contains(&q.options.len()));
                    prop_assert!(q.header.chars().count() <= HEADER_MAX_CHARS);
                    prop_assert!(q.default.is_none_or(|d| d < q.options.len()));
                }
            }
        });
    }
}
