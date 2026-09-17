//! Jev (TypeSafe System One): shared typed-decision capability.
//!
//! A third wire protocol next to OpenAI-compatible chat and Anthropic
//! messages — and not a chat model at all. One
//! `POST {base_url}/v1/systemone` call evaluates a named map of
//! [`Question`]s (`Choice`/`Score`/`Noul`) against a state value and returns
//! typed [`Answer`]s with probabilities plus confidence, no generated text.
//! Bearer auth; see <https://docs.typesafe.ai/api>.
//!
//! This module is deliberately caller-agnostic: model routing asks a
//! tier-Choice today, skill selection will ask a skill-Choice later — both
//! go through [`evaluate`]. Caller policy (which questions, confidence
//! thresholds, deterministic fallback) lives with the callers, key/endpoint
//! resolution lives in `llm::config::resolve_jev`, so setup is documented
//! once.

use std::collections::BTreeMap;
use std::time::Duration;

/// Provider-map name: the key lands in `providers.typesafe.api_key` (or
/// the `TYPESAFE_API_KEY` env var) and an explicit
/// `providers.typesafe.base_url` beats [`DEFAULT_BASE_URL`].
pub(crate) const PROVIDER_NAME: &str = "typesafe";
/// TypeSafe API host (no path — [`SYSTEMONE_PATH`] is appended per call).
pub(crate) const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";
/// SDK default alias; pin a versioned id (`jev-1.13.0`) via `DEX_JEV_MODEL`
/// once thresholds are tuned against a release.
pub(crate) const DEFAULT_MODEL: &str = "jev-latest";
/// The single SystemOne endpoint; every model is served behind it.
pub(crate) const SYSTEMONE_PATH: &str = "/v1/systemone";
/// Classifier calls must stay far under a turn's LLM latency (typical Jev
/// answers land in 70–500ms); callers fall back past this.
pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 15;

/// Resolved Jev setup, built once per call site by
/// `llm::config::resolve_jev` (key → endpoint → model precedence).
#[derive(Debug, Clone)]
pub(crate) struct JevConfig {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) model: String,
    pub(crate) timeout: Duration,
}

/// One named SystemOne question. The `id` is caller-chosen and never sent
/// to the model — the matching answer comes back under the same id.
#[derive(Debug, Clone)]
pub(crate) struct Question {
    pub(crate) id: String,
    pub(crate) kind: QuestionKind,
}

/// The three SystemOne primitives. `instructions` is the question the model
/// answers; `criteria` carries each kind's answer space: Choice takes an
/// option-name → description map (names and descriptions are both sent, so
/// descriptions must separate the options), Score an ordered 2–10 level
/// array (level number = position from 0), Noul optional yes/no glosses.
#[derive(Debug, Clone)]
pub(crate) enum QuestionKind {
    Choice {
        instructions: String,
        options: Vec<(String, String)>,
    },
    // Shared vocabulary for the next caller (skill selection rates
    // candidates on Score/Noul probes in the same call); the response side
    // already parses every kind, so constructors stay symmetric.
    #[allow(dead_code)]
    Score {
        instructions: String,
        levels: Vec<String>,
    },
    #[allow(dead_code)]
    Noul {
        instructions: String,
        yes: Option<String>,
        no: Option<String>,
    },
}

impl QuestionKind {
    fn kind_name(&self) -> &'static str {
        match self {
            QuestionKind::Choice { .. } => "choice",
            QuestionKind::Score { .. } => "score",
            QuestionKind::Noul { .. } => "noul",
        }
    }

    /// The `{"type", "instructions", "criteria"?}` object for one question.
    fn to_json(&self) -> serde_json::Value {
        match self {
            QuestionKind::Choice {
                instructions,
                options,
            } => serde_json::json!({
                "type": self.kind_name(),
                "instructions": instructions,
                "criteria": options
                    .iter()
                    .map(|(name, desc)| (name.clone(), desc.clone()))
                    .collect::<BTreeMap<_, _>>(),
            }),
            QuestionKind::Score {
                instructions,
                levels,
            } => serde_json::json!({
                "type": self.kind_name(),
                "instructions": instructions,
                "criteria": levels,
            }),
            QuestionKind::Noul {
                instructions,
                yes,
                no,
            } => {
                let mut q =
                    serde_json::json!({"type": self.kind_name(), "instructions": instructions});
                if yes.is_some() || no.is_some() {
                    q["criteria"] = serde_json::json!({
                        "true": yes.clone().unwrap_or_default(),
                        "false": no.clone().unwrap_or_default(),
                    });
                }
                q
            }
        }
    }
}

/// One typed SystemOne answer. Choice/Score carry the winner plus the full
/// distribution and a 0–1 confidence derived from it; Noul is just the
/// probability the answer is yes (callers threshold it — no separate
/// confidence exists).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Answer {
    Choice {
        value: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Score {
        value: f64,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
        legend: BTreeMap<String, String>,
    },
    Noul {
        probability_yes: f64,
    },
}

/// A parsed SystemOne response: the versioned model id that answered plus
/// one [`Answer`] per requested question id.
#[derive(Debug, Clone)]
pub(crate) struct Evaluation {
    // The versioned model id that answered (`jev-1.13.0` behind the
    // `jev-latest` alias): TypeSafe recommends logging it, since aliases
    // move. No log sink yet — the next caller records it with its decision.
    #[allow(dead_code)]
    pub(crate) model: String,
    pub(crate) answers: BTreeMap<String, Answer>,
}

/// Evaluate `questions` against `state` in one parallel SystemOne call.
/// `state` is text today (routing passes the prompt plus a turn-context
/// trailer); the field also accepts objects/arrays for later callers.
/// Every requested id must come back typed, or the whole call errors and
/// the caller falls back — a partial answer map must never route.
pub(crate) async fn evaluate(
    cfg: &JevConfig,
    state: serde_json::Value,
    questions: &[Question],
) -> Result<Evaluation, Box<dyn std::error::Error + Send + Sync>> {
    if questions.is_empty() {
        return Err("jev: no questions to evaluate".into());
    }
    let url = format!("{}{SYSTEMONE_PATH}", cfg.base_url.trim_end_matches('/'));
    let mut qmap = serde_json::Map::with_capacity(questions.len());
    for q in questions {
        qmap.insert(q.id.clone(), q.kind.to_json());
    }
    let body = serde_json::json!({
        "state": state,
        "model": cfg.model,
        "questions": qmap,
    });
    let client = reqwest::Client::builder()
        .timeout(cfg.timeout)
        .build()
        .map_err(|e| format!("jev: building client: {e}"))?;
    let resp = client
        .post(&url)
        .bearer_auth(&cfg.api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("jev: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("jev: reading response: {e}"))?;
    if !status.is_success() {
        return Err(format!("jev: HTTP {status}: {}", truncate(&text, 500)).into());
    }
    let parsed: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("jev: invalid JSON: {e}"))?;
    parse_evaluation(
        &parsed,
        &questions.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(),
    )
}

/// Strict parse of a SystemOne response: top-level `model` plus an
/// `answers` entry per requested id, each carrying its kind's documented
/// fields. Anything else (missing id, mistyped value, unknown kind)
/// errors — callers treat that as "classifier unavailable".
fn parse_evaluation(
    body: &serde_json::Value,
    expected_ids: &[&str],
) -> Result<Evaluation, Box<dyn std::error::Error + Send + Sync>> {
    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .ok_or("jev: response has no string 'model'")?
        .to_string();
    let answers = body
        .get("answers")
        .and_then(|a| a.as_object())
        .ok_or("jev: response has no 'answers' object")?;
    let mut out = BTreeMap::new();
    for id in expected_ids {
        let raw = answers
            .get(*id)
            .ok_or_else(|| format!("jev: response missing answer '{id}'"))?;
        out.insert(id.to_string(), parse_answer(id, raw)?);
    }
    Ok(Evaluation {
        model,
        answers: out,
    })
}

fn parse_answer(
    id: &str,
    raw: &serde_json::Value,
) -> Result<Answer, Box<dyn std::error::Error + Send + Sync>> {
    let err = |msg: &str| -> Box<dyn std::error::Error + Send + Sync> {
        format!("jev: answer '{id}': {msg}").into()
    };
    let kind = raw
        .get("type")
        .and_then(|t| t.as_str())
        .ok_or_else(|| err("no string 'type'"))?;
    let probabilities = |raw: &serde_json::Value| -> Result<BTreeMap<String, f64>, _> {
        let map = raw
            .get("probabilities")
            .and_then(|p| p.as_object())
            .ok_or_else(|| err("no 'probabilities' object"))?;
        map.iter()
            .map(|(k, v)| {
                v.as_f64()
                    .map(|f| (k.clone(), f))
                    .ok_or_else(|| err("non-numeric probability"))
            })
            .collect()
    };
    let confidence = |raw: &serde_json::Value| {
        raw.get("confidence")
            .and_then(|c| c.as_f64())
            .ok_or_else(|| err("no numeric 'confidence'"))
    };
    match kind {
        "choice" => {
            let value = raw
                .get("choice")
                .and_then(|c| c.as_str())
                .ok_or_else(|| err("no string 'choice'"))?
                .to_string();
            Ok(Answer::Choice {
                value,
                probabilities: probabilities(raw)?,
                confidence: confidence(raw)?,
            })
        }
        "score" => {
            let value = raw
                .get("score")
                .and_then(|s| s.as_f64())
                .ok_or_else(|| err("no numeric 'score'"))?;
            let legend = raw
                .get("legend")
                .and_then(|l| l.as_object())
                .map(|l| {
                    l.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                        .collect()
                })
                .unwrap_or_default();
            Ok(Answer::Score {
                value,
                probabilities: probabilities(raw)?,
                confidence: confidence(raw)?,
                legend,
            })
        }
        "noul" => {
            let probability_yes = raw
                .get("noul")
                .and_then(|n| n.as_f64())
                .ok_or_else(|| err("no numeric 'noul'"))?;
            if !(0.0..=1.0).contains(&probability_yes) {
                return Err(err("noul outside 0–1"));
            }
            Ok(Answer::Noul { probability_yes })
        }
        other => Err(err(&format!("unknown type '{other}'"))),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg(base_url: &str) -> JevConfig {
        JevConfig {
            base_url: base_url.to_string(),
            api_key: "test-key".to_string(),
            model: DEFAULT_MODEL.to_string(),
            timeout: Duration::from_secs(10),
        }
    }

    fn tier_question() -> Question {
        Question {
            id: "tier".to_string(),
            kind: QuestionKind::Choice {
                instructions: "Which tier?".to_string(),
                options: vec![
                    ("fast".to_string(), "trivial".to_string()),
                    ("balanced".to_string(), "normal".to_string()),
                    ("powerful".to_string(), "hard".to_string()),
                ],
            },
        }
    }

    /// Minimal SystemOne mock: asserts bearer auth, then answers every
    /// requested id from `answers` (a JSON object literal per id).
    async fn spawn_mock(
        answers: serde_json::Value,
        seen_body: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>,
    ) -> String {
        use axum::extract::State as AxumState;
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::Router;

        #[derive(Clone)]
        struct AppState {
            answers: serde_json::Value,
            seen_body: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>,
        }

        async fn handler(
            AxumState(st): AxumState<AppState>,
            req: axum::extract::Request,
        ) -> axum::response::Response {
            let auth_ok = req
                .headers()
                .get("authorization")
                .is_some_and(|v| v == "Bearer test-key");
            let (_, body) = req.into_parts();
            let bytes = axum::body::to_bytes(body, 64 * 1024).await.unwrap();
            let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            *st.seen_body.lock().unwrap() = Some(parsed.clone());
            if !auth_ok {
                return axum::http::StatusCode::UNAUTHORIZED.into_response();
            }
            // Echo back only the requested ids, like the real endpoint.
            let empty = serde_json::Map::new();
            let asked = parsed
                .get("questions")
                .and_then(|q| q.as_object())
                .unwrap_or(&empty);
            let mut out = serde_json::Map::new();
            for id in asked.keys() {
                if let Some(a) = st.answers.get(id) {
                    out.insert(id.clone(), a.clone());
                }
            }
            axum::Json(serde_json::json!({
                "model": "jev-1.13.0",
                "answers": out,
                "usage": {"input_tokens": 10, "output_tokens": 4},
            }))
            .into_response()
        }

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let state = AppState { answers, seen_body };
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/systemone", post(handler))
                    .with_state(state),
            )
            .await
            .unwrap();
        });
        format!("http://{addr}")
    }

    fn seen() -> std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>> {
        std::sync::Arc::new(std::sync::Mutex::new(None))
    }

    #[tokio::test]
    async fn evaluate_posts_choice_and_parses_answer() {
        let seen_body = seen();
        let base = spawn_mock(
            serde_json::json!({
                "tier": {"type": "choice", "choice": "fast",
                         "probabilities": {"fast": 0.85, "balanced": 0.1, "powerful": 0.05},
                         "confidence": 0.82}
            }),
            seen_body.clone(),
        )
        .await;
        let eval = evaluate(
            &test_cfg(&base),
            serde_json::json!("fix typo"),
            &[tier_question()],
        )
        .await
        .unwrap();
        assert_eq!(eval.model, "jev-1.13.0");
        assert_eq!(
            eval.answers.get("tier"),
            Some(&Answer::Choice {
                value: "fast".to_string(),
                probabilities: [("fast", 0.85), ("balanced", 0.1), ("powerful", 0.05)]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
                confidence: 0.82,
            })
        );
        // Request shape: state + model + named questions map with criteria.
        let sent = seen_body.lock().unwrap().clone().unwrap();
        assert_eq!(sent["state"], serde_json::json!("fix typo"));
        assert_eq!(sent["model"], serde_json::json!("jev-latest"));
        assert_eq!(
            sent["questions"]["tier"]["type"],
            serde_json::json!("choice")
        );
        assert_eq!(
            sent["questions"]["tier"]["criteria"]["fast"],
            serde_json::json!("trivial")
        );
    }

    #[tokio::test]
    async fn evaluate_parses_score_and_noul() {
        let base = spawn_mock(
            serde_json::json!({
                "sev": {"type": "score", "score": 1.3, "confidence": 0.54,
                        "legend": {"0": "cosmetic", "1": "broken", "2": "blocking"},
                        "probabilities": {"0": 0.0, "1": 0.7, "2": 0.3}},
                "urgent": {"type": "noul", "noul": 0.92}
            }),
            seen(),
        )
        .await;
        let eval = evaluate(
            &test_cfg(&base),
            serde_json::json!("payouts failing"),
            &[
                Question {
                    id: "sev".to_string(),
                    kind: QuestionKind::Score {
                        instructions: "How severe?".to_string(),
                        levels: vec![
                            "cosmetic".to_string(),
                            "broken".to_string(),
                            "blocking".to_string(),
                        ],
                    },
                },
                Question {
                    id: "urgent".to_string(),
                    kind: QuestionKind::Noul {
                        instructions: "Urgent?".to_string(),
                        yes: None,
                        no: None,
                    },
                },
            ],
        )
        .await
        .unwrap();
        assert!(matches!(
            eval.answers.get("sev"),
            Some(Answer::Score { value, confidence, .. })
                if (*value - 1.3).abs() < f64::EPSILON && (*confidence - 0.54).abs() < f64::EPSILON
        ));
        assert_eq!(
            eval.answers.get("urgent"),
            Some(&Answer::Noul {
                probability_yes: 0.92
            })
        );
    }

    #[tokio::test]
    async fn evaluate_errors_on_missing_answer_and_http_failure() {
        // Mock answers nothing → missing-id error, never a partial map.
        let base = spawn_mock(serde_json::json!({}), seen()).await;
        let err = evaluate(&test_cfg(&base), serde_json::json!("x"), &[tier_question()])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing answer 'tier'"), "{err}");
        // Wrong key → the mock 401s, surfacing an HTTP error.
        let mut cfg = test_cfg(&base);
        cfg.api_key = "wrong".to_string();
        let err = evaluate(&cfg, serde_json::json!("x"), &[tier_question()])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("HTTP 401"), "{err}");
    }

    #[test]
    fn evaluate_rejects_empty_questions_without_network() {
        let cfg = test_cfg("http://127.0.0.1:1");
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(evaluate(&cfg, serde_json::json!("x"), &[]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no questions"), "{err}");
    }

    #[test]
    fn question_serialization_matches_api_reference() {
        // Field names follow https://docs.typesafe.ai/api (`instructions`,
        // `criteria`; Choice criteria is a name → description map, Score an
        // ordered level array, Noul criteria optional when unglossed).
        let choice = QuestionKind::Choice {
            instructions: "Which team?".to_string(),
            options: vec![("billing".to_string(), "money stuff".to_string())],
        }
        .to_json();
        assert_eq!(choice["type"], serde_json::json!("choice"));
        assert_eq!(
            choice["criteria"]["billing"],
            serde_json::json!("money stuff")
        );
        let score = QuestionKind::Score {
            instructions: "How severe?".to_string(),
            levels: vec!["low".to_string(), "high".to_string()],
        }
        .to_json();
        assert_eq!(score["criteria"], serde_json::json!(["low", "high"]));
        let noul = QuestionKind::Noul {
            instructions: "Urgent?".to_string(),
            yes: None,
            no: None,
        }
        .to_json();
        assert!(noul.get("criteria").is_none());
    }
}
