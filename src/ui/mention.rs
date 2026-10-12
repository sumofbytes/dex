//! `@` file/folder mentions in the composer: token detection at the cursor,
//! the popup's candidate list, and completion. Candidates come from the
//! daemon (`GET /api/files`) — its index, its workspace — so the popup stays
//! right under `dex connect`. The popup itself is the slash popup's sheet
//! (`slash_suggestions` returns these rows while a mention is active).

use super::App;
use super::InputField;
use crate::protocol::FileMatch;
use std::time::{Duration, Instant};

/// Quiet time after the last keystroke before the daemon is asked again.
#[cfg(not(test))]
const DEBOUNCE: Duration = Duration::from_millis(80);
#[cfg(test)]
const DEBOUNCE: Duration = Duration::ZERO;

/// Rows requested per query; the sheet scrolls past its visible cap.
pub(crate) const MENTION_LIMIT: usize = 30;

/// The `@token` ending at the cursor.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Token {
    pub(crate) row: usize,
    /// Byte offset of the `@`.
    pub(crate) start: usize,
    /// Text typed after the `@` (without an opening quote).
    pub(crate) query: String,
    pub(crate) quoted: bool,
}

#[derive(Default)]
pub(crate) struct MentionState {
    /// Query the daemon was last asked about (dedupes requests).
    pub(crate) requested: Option<String>,
    /// Results for `requested`; cleared whenever the query changes.
    pub(crate) results: Vec<FileMatch>,
    /// `(row, start)` of a token Esc closed; typing within it stays closed.
    pub(crate) dismissed: Option<(usize, usize)>,
    /// A changed query waiting out the debounce, and since when.
    pending: Option<(String, Instant)>,
}

/// An `@` at the line start or after whitespace, up to the cursor, with no
/// whitespace in between (unless quoted: `@"my dir/…`).
pub(crate) fn token_at_cursor(input: &InputField) -> Option<Token> {
    let line = input.lines.get(input.row)?;
    let col = input.col.min(line.len());
    if !line.is_char_boundary(col) {
        return None;
    }
    let before = &line[..col];
    let start = before
        .char_indices()
        .rev()
        .find(|&(i, c)| {
            c == '@'
                && before[..i]
                    .chars()
                    .next_back()
                    .is_none_or(char::is_whitespace)
        })
        .map(|(i, _)| i)?;
    let typed = &before[start + 1..];
    let (quoted, query) = match typed.strip_prefix('"') {
        Some(rest) if rest.contains('"') => return None,
        Some(rest) => (true, rest),
        None if typed.contains(char::is_whitespace) => return None,
        None => (false, typed),
    };
    Some(Token {
        row: input.row,
        start,
        query: query.to_string(),
        quoted,
    })
}

/// The live token, unless popups are off right now (history walk, slash
/// command line) or Esc closed it. Stays on while a turn runs so a steer or
/// follow-up can reference files too.
pub(crate) fn active_token(app: &App) -> Option<Token> {
    if app.history_index.is_some() || app.input.text().starts_with('/') {
        return None;
    }
    let token = token_at_cursor(&app.input)?;
    (app.mention.dismissed != Some((token.row, token.start))).then_some(token)
}

/// Whether the `@` popup is showing rows (it then owns Esc/Tab/Enter).
pub(crate) fn is_open(app: &App) -> bool {
    !suggestions(app).is_empty()
}

/// Popup rows as `(label, description)`, folders marked with a trailing `/`.
pub(crate) fn suggestions(app: &App) -> Vec<(String, String)> {
    if active_token(app).is_none() {
        return Vec::new();
    }
    app.mention
        .results
        .iter()
        .map(|m| {
            if m.is_dir {
                (format!("{}/", m.path), "folder".to_string())
            } else {
                (m.path.clone(), String::new())
            }
        })
        .collect()
}

/// Replace the token with the highlighted candidate. Files get a trailing
/// space; folders end in `/` so the popup keeps drilling down.
pub(crate) fn complete(app: &mut App) -> bool {
    let Some(token) = active_token(app) else {
        return false;
    };
    let rows = suggestions(app);
    let Some((label, _)) = rows.get(app.slash_selected.min(rows.len().saturating_sub(1))) else {
        return false;
    };
    let is_dir = label.ends_with('/');
    let mention = if label.contains(char::is_whitespace) {
        // Quoted folders stay open (no closing quote) so drilling continues.
        if is_dir {
            format!("@\"{label}")
        } else {
            format!("@\"{label}\"")
        }
    } else {
        format!("@{label}")
    };
    let line = &mut app.input.lines[token.row];
    let end = app.input.col.min(line.len());
    line.replace_range(token.start..end, &mention);
    app.input.col = token.start + mention.len();
    if !is_dir {
        app.input.insert_char(' ');
    }
    app.slash_selected = 0;
    true
}

/// Esc: close the popup for this token only.
pub(crate) fn dismiss(app: &mut App) -> bool {
    let Some(token) = active_token(app) else {
        return false;
    };
    app.mention.dismissed = Some((token.row, token.start));
    app.slash_selected = 0;
    true
}

/// The query the daemon should be asked about now, if it differs from the
/// last one asked. Also forgets a stale dismissal once the cursor leaves it.
pub(crate) fn query_to_request(app: &mut App) -> Option<String> {
    query_to_request_after(app, DEBOUNCE)
}

/// Whether a changed query is still waiting out the debounce (the event loop
/// polls faster meanwhile so the request isn't left to the idle tick).
pub(crate) fn debouncing(app: &App) -> bool {
    app.mention.pending.is_some()
}

fn query_to_request_after(app: &mut App, debounce: Duration) -> Option<String> {
    let token = if app.history_index.is_some() || app.input.text().starts_with('/') {
        None
    } else {
        token_at_cursor(&app.input)
    };
    let Some(token) = token else {
        app.mention.dismissed = None;
        app.mention.requested = None;
        app.mention.pending = None;
        app.mention.results.clear();
        return None;
    };
    if app.mention.dismissed != Some((token.row, token.start)) {
        app.mention.dismissed = None;
    } else {
        return None;
    }
    if app.mention.requested.as_deref() == Some(token.query.as_str()) {
        app.mention.pending = None;
        return None;
    }
    // Rows for the previous query would capture Enter/Tab for the wrong path.
    app.mention.results.clear();
    // The first query for a token goes out at once; later edits wait until
    // typing pauses, so a burst costs one lookup instead of one per key.
    if app.mention.requested.is_some() && !debounce.is_zero() {
        let since = match &app.mention.pending {
            Some((q, t)) if *q == token.query => *t,
            _ => {
                app.mention.pending = Some((token.query, Instant::now()));
                return None;
            }
        };
        if since.elapsed() < debounce {
            return None;
        }
    }
    app.mention.pending = None;
    app.mention.requested = Some(token.query.clone());
    Some(token.query)
}

/// Apply a reply; ignored when the user has typed past the query it answers.
pub(crate) fn apply_results(app: &mut App, query: &str, files: Vec<FileMatch>) -> bool {
    if app.mention.requested.as_deref() != Some(query) {
        return false;
    }
    app.mention.results = files;
    app.slash_selected = 0;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(text: &str) -> InputField {
        InputField::from_text(text)
    }

    #[test]
    fn token_needs_a_whitespace_led_at() {
        let t = token_at_cursor(&field("look at @src/ma")).unwrap();
        assert_eq!((t.start, t.query.as_str(), t.quoted), (8, "src/ma", false));
        assert!(token_at_cursor(&field("mail me@x.com")).is_none());
        assert!(token_at_cursor(&field("@a done")).is_none());
        assert_eq!(token_at_cursor(&field("@")).unwrap().query, "");
    }

    #[test]
    fn quoted_token_allows_spaces_until_closed() {
        let t = token_at_cursor(&field("see @\"my di")).unwrap();
        assert_eq!((t.query.as_str(), t.quoted), ("my di", true));
        assert!(token_at_cursor(&field("see @\"my dir\" x")).is_none());
    }

    fn answered(app: &mut App, text: &str, query: &str, files: &[(&str, bool)]) {
        app.input = field(text);
        assert_eq!(query_to_request(app).as_deref(), Some(query));
        let files = files
            .iter()
            .map(|(path, is_dir)| FileMatch {
                path: path.to_string(),
                is_dir: *is_dir,
            })
            .collect();
        assert!(apply_results(app, query, files));
    }

    #[test]
    fn completing_a_file_closes_with_a_space_and_a_folder_keeps_drilling() {
        let mut app = App::test_app();
        answered(
            &mut app,
            "read @sr",
            "sr",
            &[("src", true), ("src/a.rs", false)],
        );
        assert_eq!(
            suggestions(&app),
            vec![
                ("src/".to_string(), "folder".to_string()),
                ("src/a.rs".to_string(), String::new())
            ]
        );
        assert!(complete(&mut app));
        assert_eq!(app.input.text(), "read @src/");
        app.slash_selected = 1;
        assert!(complete(&mut app));
        assert_eq!(app.input.text(), "read @src/a.rs ");
        // The cursor left the token: no popup, no stale request.
        assert!(suggestions(&app).is_empty());
        assert_eq!(query_to_request(&mut app), None);
    }

    #[test]
    fn paths_with_spaces_are_quoted_and_late_replies_dropped() {
        let mut app = App::test_app();
        answered(&mut app, "@my", "my", &[("my dir/f.txt", false)]);
        assert!(complete(&mut app));
        assert_eq!(app.input.text(), "@\"my dir/f.txt\" ");
        app.input = field("@zz");
        assert_eq!(query_to_request(&mut app).as_deref(), Some("zz"));
        assert!(!apply_results(&mut app, "my", Vec::new()));
    }

    #[test]
    fn edits_wait_for_a_typing_pause_but_the_first_query_does_not() {
        let mut app = App::test_app();
        let pause = Duration::from_millis(30);
        app.input = field("@s");
        assert_eq!(
            query_to_request_after(&mut app, pause).as_deref(),
            Some("s")
        );
        app.input = field("@sr");
        assert_eq!(query_to_request_after(&mut app, pause), None);
        assert!(debouncing(&app));
        app.input = field("@src");
        assert_eq!(query_to_request_after(&mut app, pause), None);
        std::thread::sleep(pause * 2);
        assert_eq!(
            query_to_request_after(&mut app, pause).as_deref(),
            Some("src")
        );
        assert!(!debouncing(&app));
    }

    #[test]
    fn esc_closes_only_the_current_token() {
        let mut app = App::test_app();
        answered(&mut app, "@a", "a", &[("a.rs", false)]);
        assert!(dismiss(&mut app));
        assert!(suggestions(&app).is_empty());
        app.input = field("@ab");
        assert_eq!(query_to_request(&mut app), None);
        app.input = field("@ab @c");
        assert_eq!(query_to_request(&mut app).as_deref(), Some("c"));
    }

    #[test]
    fn popup_stays_available_while_a_turn_runs() {
        let mut app = App::test_app();
        app.busy = true;
        answered(&mut app, "also check @ma", "ma", &[("main.rs", false)]);
        assert_eq!(suggestions(&app).len(), 1);
        assert!(complete(&mut app));
        assert_eq!(app.input.text(), "also check @main.rs ");
    }
}
