//! MCP OAuth login for Streamable-HTTP servers: protected-resource
//! metadata (RFC9728) + authorization-server metadata (RFC8414) + dynamic
//! client registration (RFC7591) + PKCE S256 (RFC7636), loopback redirect.
//!
//! `dex mcp login <server>` exchanges a browser approval for tokens stored
//! in `$XDG_DATA_HOME/dex/mcp/<server>.json` (0600, never in sessions or
//! logs). [`super::HttpTransport`] injects a valid token per request and
//! turns a `Bearer` 401 into a login hint; a stored refresh token is tried
//! once before the hint surfaces.

mod flow;
mod storage;
#[cfg(test)]
pub(crate) use flow::{
    authorize_url, code_challenge, ensure_https_or_loopback, exchange_code,
    fetch_auth_server_metadata, fetch_resource_metadata, parse_callback,
    parse_resource_metadata_url, percent_decode, percent_encode, pkce_pair, probe_challenge,
    register_client, split_origin, token_from_response, wait_for_callback, well_known_resource_url,
};
pub(crate) use flow::{
    clear_refresh_backoff, is_bearer_challenge, login, logout, note_refresh_failure,
    refresh_access_token, refresh_backoff_active,
};
pub(crate) use storage::{
    auth_line_with, auth_lines, clear_token, load_token, save_token, valid_token,
};
#[cfg(test)]
pub(crate) use storage::{now_secs, status_line, token_path, OAuthToken};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc7636_vector() {
        // The RFC's printed Appendix B challenge drops the final char (42
        // chars cannot encode a 32-byte digest); the true value ends `-cM`.
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let (verifier, challenge) = pkce_pair();
        assert_eq!(verifier.len(), 43);
        assert_eq!(code_challenge(&verifier), challenge);
    }

    #[test]
    fn resource_metadata_url_parses() {
        assert_eq!(
            parse_resource_metadata_url(
                r#"Bearer resource_metadata="https://auth.example.com/.well-known/oauth-protected-resource""#
            )
            .as_deref(),
            Some("https://auth.example.com/.well-known/oauth-protected-resource")
        );
        // Extra params, odd spacing/case.
        assert_eq!(
            parse_resource_metadata_url(
                r#"Bearer error="invalid_token", Resource_Metadata = "https://a.example/x", scope="mcp""#
            )
            .as_deref(),
            Some("https://a.example/x")
        );
        assert_eq!(parse_resource_metadata_url("Bearer"), None);
        assert_eq!(parse_resource_metadata_url("Basic realm=\"x\""), None);
        // Non-URL values are ignored, not trusted.
        assert_eq!(
            parse_resource_metadata_url(r#"Bearer resource_metadata="not-a-url""#),
            None
        );
    }

    #[test]
    fn url_codec_roundtrips() {
        assert_eq!(
            percent_encode("http://127.0.0.1:9/cb?a=b c"),
            "http%3A%2F%2F127.0.0.1%3A9%2Fcb%3Fa%3Db%20c"
        );
        // RFC3986: `+` is literal, only `%XX` decodes (form `+`→space used
        // to corrupt codes in the callback query).
        assert_eq!(percent_decode("a%20b+c%2F"), "a b+c/");
        assert_eq!(
            percent_decode(percent_encode("code/x+y=z&").as_str()),
            "code/x+y=z&"
        );
    }

    #[test]
    fn bearer_challenge_is_scheme_not_substring() {
        assert!(is_bearer_challenge("Bearer"));
        assert!(is_bearer_challenge(
            r#"Bearer resource_metadata="https://a/x""#
        ));
        assert!(is_bearer_challenge(
            r#"Basic realm="x", Bearer error="invalid_token""#
        ));
        assert!(!is_bearer_challenge("Basic realm=\"x\""));
        assert!(!is_bearer_challenge("Bearertoken xyz"));
        assert!(!is_bearer_challenge(""));
    }

    #[test]
    fn https_required_except_loopback() {
        assert!(ensure_https_or_loopback("https://a.example/token", "t").is_ok());
        assert!(ensure_https_or_loopback("http://127.0.0.1:8080/x", "t").is_ok());
        assert!(ensure_https_or_loopback("http://localhost:9/x", "t").is_ok());
        assert!(ensure_https_or_loopback("http://a.example/token", "t").is_err());
        assert!(ensure_https_or_loopback("http://a.example/token", "issuer").is_err());
    }

    #[test]
    fn token_path_never_traverses() {
        let p = token_path("../../etc/passwd");
        assert_eq!(
            p.file_name().and_then(|n| n.to_str()),
            Some("______etc_passwd.json")
        );
        assert!(p.parent().is_some_and(|d| d.ends_with("dex/mcp")));
    }

    #[test]
    fn origin_splits() {
        assert_eq!(
            split_origin("https://h.example:8443/a/b"),
            ("https://h.example:8443".to_string(), "/a/b".to_string())
        );
        assert_eq!(
            split_origin("https://h.example"),
            ("https://h.example".to_string(), String::new())
        );
        // Query/fragment never leak into derived well-known URLs.
        assert_eq!(
            split_origin("https://h.example/mcp?x=1#frag"),
            ("https://h.example".to_string(), "/mcp".to_string())
        );
        assert_eq!(
            well_known_resource_url("https://h.example/mcp"),
            "https://h.example/.well-known/oauth-protected-resource/mcp"
        );
        assert_eq!(
            well_known_resource_url("https://h.example/mcp?x=1"),
            "https://h.example/.well-known/oauth-protected-resource/mcp"
        );
    }

    #[test]
    fn callback_parses_code_and_rejects_denial() {
        let (code, state) =
            parse_callback("GET /callback?code=abc%201&state=s7 HTTP/1.1\r\nHost: x\r\n").unwrap();
        assert_eq!((code.as_str(), state.as_str()), ("abc 1", "s7"));
        // `+` in a code is literal (RFC3986), not a space.
        let (code, _) = parse_callback("GET /callback?code=a%2Bb+c&state=s HTTP/1.1\r\n").unwrap();
        assert_eq!(code, "a+b+c");
        // Error code is preserved so login can retry without `resource` on
        // `invalid_target`.
        let err =
            parse_callback("GET /callback?error=invalid_target&error_description=nope HTTP/1.1")
                .unwrap_err();
        assert!(err.contains("invalid_target"), "{err}");
        assert!(parse_callback("GET /callback?error=access_denied HTTP/1.1").is_err());
        assert!(parse_callback("POST /callback?code=x HTTP/1.1").is_err());
    }

    #[test]
    fn token_store_roundtrips_hermetic() {
        let _lock = super::super::TEST_ENV_LOCK.blocking_lock();
        let prev = std::env::var_os("XDG_DATA_HOME");
        let dir = std::env::temp_dir().join(format!("dex-oauth-{}", std::process::id()));
        std::env::set_var("XDG_DATA_HOME", &dir);
        let tok = OAuthToken {
            access_token: "a".to_string(),
            refresh_token: Some("r".to_string()),
            expires_at: Some(now_secs() + 3600),
            token_endpoint: "https://a.example/token".to_string(),
            client_id: "c".to_string(),
            client_secret: None,
        };
        assert!(save_token("roundtrip", &tok).is_ok());
        assert!(valid_token("roundtrip").is_some());
        assert!(status_line("roundtrip").contains("logged in"));
        assert!(clear_token("roundtrip"));
        assert!(load_token("roundtrip").is_none());
        assert!(status_line("roundtrip").contains("not logged in"));
        match prev {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expired_tokens_are_not_valid() {
        let tok = OAuthToken {
            access_token: "a".to_string(),
            refresh_token: None,
            expires_at: Some(now_secs() - 10),
            token_endpoint: String::new(),
            client_id: String::new(),
            client_secret: None,
        };
        assert!(!tok.usable());
        assert!(!tok.refreshable());
    }
}

#[cfg(test)]
mod flow_tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::{Path, State};
    use axum::http::Response;
    use axum::routing::{get, post};
    use axum::Router;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    type Routes = std::sync::RwLock<BTreeMap<String, (u16, String, String)>>;

    /// A tiny OAuth provider mock: path -> (status, content-type, body), plus
    /// a log of every request so tests can assert on what was sent.
    #[derive(Default)]
    struct Mock {
        routes: Routes,
        seen: std::sync::Mutex<Vec<String>>,
    }

    async fn spawn_mock(mock: Arc<Mock>) -> String {
        let app = Router::new().route(
            "/{*rest}",
            get(
                |State(m): State<Arc<Mock>>, path: Path<String>| async move {
                    m.seen.lock().unwrap().push(format!("GET {}", path.0));
                    reply(&m, &path.0)
                },
            )
            .post(
                |State(m): State<Arc<Mock>>, path: Path<String>, body: String| async move {
                    m.seen
                        .lock()
                        .unwrap()
                        .push(format!("POST {} {body}", path.0));
                    reply(&m, &path.0)
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let app = app.with_state(mock.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn reply(mock: &Mock, path: &str) -> Response<Body> {
        let routes = mock.routes.read().unwrap();
        let Some((status, content_type, body)) = routes.get(path) else {
            return Response::builder()
                .status(404)
                .body(Body::from("no route"))
                .unwrap();
        };
        Response::builder()
            .status(*status)
            .header("content-type", content_type.clone())
            .body(Body::from(body.clone()))
            .unwrap()
    }

    fn set_route(mock: &Mock, path: &str, status: u16, content_type: &str, body: &str) {
        // The `/{rest}` capture drops the leading slash.
        let key = path.trim_start_matches('/').to_string();
        mock.routes
            .write()
            .unwrap()
            .insert(key, (status, content_type.to_string(), body.to_string()));
    }

    fn shared() -> reqwest::Client {
        crate::client::http::shared_async_client()
    }

    fn mk_tok(refresh: Option<&str>) -> OAuthToken {
        OAuthToken {
            access_token: "old-access".into(),
            refresh_token: refresh.map(str::to_string),
            expires_at: Some(now_secs() + 3600),
            token_endpoint: String::new(),
            client_id: "client-1".into(),
            client_secret: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn register_and_exchange_against_local_mock() {
        let mock = Arc::new(Mock::default());
        set_route(
            &mock,
            "/register",
            200,
            "application/json",
            r#"{"client_id":"cid-1","client_secret":"sec"}"#,
        );
        set_route(
            &mock,
            "/token",
            200,
            "application/json",
            r#"{"access_token":"at","refresh_token":"rt","expires_in":"3600"}"#,
        );
        let base = spawn_mock(mock.clone()).await;

        // Loopback http is allowed; remote https is enforced elsewhere.
        let reg = register_client(
            &shared(),
            &format!("{base}/register"),
            "http://127.0.0.1:0/cb",
        )
        .await
        .unwrap();
        assert_eq!(reg.client_id, "cid-1");
        assert_eq!(reg.client_secret.as_deref(), Some("sec"));

        let tok = exchange_code(
            &shared(),
            &format!("{base}/token"),
            "cid-1",
            Some("sec"),
            "the-code",
            "http://127.0.0.1:0/cb",
            "the-verifier",
        )
        .await
        .unwrap();
        assert_eq!(tok.access_token, "at");
        assert_eq!(tok.refresh_token.as_deref(), Some("rt"));
        assert!(
            tok.expires_at.is_some(),
            "expires_in parses even as a string"
        );
        assert!(
            mock.seen
                .lock()
                .unwrap()
                .iter()
                .any(|s| s.contains("code=the-code") && s.contains("code_verifier=the-verifier")),
            "the exchange posts the code + PKCE verifier"
        );

        // Failure shape surfaces the server text, never a panic.
        set_route(&mock, "/token-fail", 400, "text/plain", "bad_grant");
        let err = exchange_code(
            &shared(),
            &format!("{base}/token-fail"),
            "cid-1",
            None,
            "bad",
            "cb",
            "v",
        )
        .await;
        let err = err.unwrap_err();
        assert!(err.contains("code exchange failed"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refresh_access_token_against_local_mock() {
        let mock = Arc::new(Mock::default());
        // Success: no new refresh_token -> the old one is kept.
        set_route(
            &mock,
            "/token",
            200,
            "application/json",
            r#"{"access_token":"new-access","expires_in":7200}"#,
        );
        let base = spawn_mock(mock.clone()).await;
        let mut saved = mk_tok(Some("rt-1"));
        saved.token_endpoint = format!("{base}/token");
        let refreshed = refresh_access_token(&saved).await.unwrap();
        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(
            refreshed.refresh_token.as_deref(),
            Some("rt-1"),
            "a refresh response without refresh_token keeps the old one"
        );
        assert_eq!(refreshed.token_endpoint, saved.token_endpoint);
        assert_eq!(refreshed.client_id, "client-1");
        assert!(
            mock.seen
                .lock()
                .unwrap()
                .iter()
                .any(|s| s.contains("grant_type=refresh_token")),
            "the refresh grant hits the token endpoint"
        );

        // invalid_grant means the stored token is dead.
        set_route(
            &mock,
            "/dead",
            400,
            "application/json",
            r#"{"error":"invalid_grant"}"#,
        );
        let mut dead = mk_tok(Some("r"));
        dead.token_endpoint = format!("{base}/dead");
        let err = refresh_access_token(&dead).await.err().unwrap();
        assert!(err.contains("invalid_grant"), "{err}");

        // Generic failure surfaces the text.
        set_route(&mock, "/broken", 500, "text/plain", "boom");
        let mut broken = mk_tok(Some("r"));
        broken.token_endpoint = format!("{base}/broken");
        let err = refresh_access_token(&broken).await.err().unwrap();
        assert!(err.contains("refresh failed"), "{err}");

        // A non-refreshable token fails fast without any HTTP.
        let err = refresh_access_token(&mk_tok(None)).await.err().unwrap();
        assert!(err.contains("not refreshable"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_for_callback_accepts_code_and_rejects_state_and_errors() {
        use tokio::io::AsyncWriteExt as _;

        // Happy path: the browser posts `GET /?code=X&state=Y` to the listener.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let talker = tokio::spawn(async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(b"GET /cb?code=abc%20d&state=s1 HTTP/1.1\r\nhost: x\r\n\r\n")
                .await
                .unwrap();
        });
        let code = wait_for_callback(&listener, "s1").await.unwrap();
        assert_eq!(code, "abc d", "percent-encoded codes decode");
        talker.await.unwrap();

        // State mismatch is a CSRF abort.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let talker = tokio::spawn(async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(b"GET /cb?code=abc&state=evil HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
        });
        let err = wait_for_callback(&listener, "s1").await.err().unwrap();
        assert!(err.contains("state mismatch"), "{err}");
        talker.await.unwrap();

        // The AS denied the authorization: the code is surfaced.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let talker = tokio::spawn(async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(
                b"GET /cb?error=access_denied&error_description=nope&state=s1 HTTP/1.1\r\n\r\n",
            )
            .await
            .unwrap();
        });
        let err = wait_for_callback(&listener, "s1").await.err().unwrap();
        assert!(err.contains("authorization denied"), "{err}");
        talker.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn probe_challenge_detects_bearer_and_idle() {
        // 401 always means auth; the WWW-Authenticate header is parsed.
        let metadata_url = "http://127.0.0.1:9/.well-known/x";
        let www = format!(r#"Bearer resource_metadata="{metadata_url}""#);
        let app = Router::new().route(
            "/mcp",
            post(move || {
                let www = www.clone();
                async move {
                    Response::builder()
                        .status(401)
                        .header("www-authenticate", www)
                        .body(Body::from("unauthorized"))
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let probe = probe_challenge(
            &shared(),
            &format!("http://{addr}/mcp"),
            &BTreeMap::default(),
        )
        .await;
        assert!(probe.challenged, "a 401 always means auth");
        assert_eq!(probe.resource_metadata.as_deref(), Some(metadata_url));

        // 200 without a challenge is idle.
        let app = Router::new().route(
            "/mcp",
            post(|| async {
                Response::builder()
                    .status(200)
                    .body(Body::from("ok"))
                    .unwrap()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let probe = probe_challenge(
            &shared(),
            &format!("http://{addr}/mcp"),
            &BTreeMap::default(),
        )
        .await;
        assert!(!probe.challenged);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn metadata_discovery_against_local_mock() {
        // Resource metadata doc parses (RFC 9728).
        let mock = Arc::new(Mock::default());
        set_route(
            &mock,
            "/.well-known/oauth-protected-resource",
            200,
            "application/json",
            r#"{"authorization_servers":["http://127.0.0.1:1/"],"scopes_supported":["mcp:read"]}"#,
        );
        let base = spawn_mock(mock).await;
        let meta = fetch_resource_metadata(
            &shared(),
            &format!("{base}/.well-known/oauth-protected-resource"),
        )
        .await
        .unwrap();
        assert_eq!(
            meta.authorization_servers,
            vec!["http://127.0.0.1:1/".to_string()]
        );
        assert_eq!(meta.scopes, vec!["mcp:read".to_string()]);

        // Missing authorization_servers is a clear error.
        let mock = Arc::new(Mock::default());
        set_route(
            &mock,
            "/.well-known/oauth-protected-resource",
            200,
            "application/json",
            r#"{"scopes_supported":[]}"#,
        );
        let base = spawn_mock(mock).await;
        let err = fetch_resource_metadata(
            &shared(),
            &format!("{base}/.well-known/oauth-protected-resource"),
        )
        .await
        .err()
        .unwrap();
        assert!(err.contains("no authorization_servers"), "{err}");

        // Auth-server metadata discovery falls back across well-known paths:
        // only the openid-configuration candidate answers.
        let mock = Arc::new(Mock::default());
        set_route(
            &mock,
            "/.well-known/openid-configuration",
            200,
            "application/json",
            &serde_json::json!({
                "authorization_endpoint": "http://127.0.0.1:1/auth",
                "token_endpoint": "http://127.0.0.1:1/token",
                "registration_endpoint": "http://127.0.0.1:1/register",
            })
            .to_string(),
        );
        let base = spawn_mock(mock).await;
        let meta = fetch_auth_server_metadata(&shared(), &base).await.unwrap();
        assert_eq!(meta.authorization_endpoint, "http://127.0.0.1:1/auth");
        assert_eq!(meta.token_endpoint, "http://127.0.0.1:1/token");
        assert_eq!(
            meta.registration_endpoint.as_deref(),
            Some("http://127.0.0.1:1/register")
        );

        // Metadata that is missing endpoints keeps looking and fails with the
        // last reason.
        let mock = Arc::new(Mock::default());
        set_route(
            &mock,
            "/.well-known/openid-configuration",
            200,
            "application/json",
            r#"{"authorization_endpoint":"http://127.0.0.1:1/auth"}"#,
        );
        let base = spawn_mock(mock).await;
        let err = fetch_auth_server_metadata(&shared(), &base)
            .await
            .err()
            .unwrap();
        assert!(err.contains("metadata not found"), "{err}");
    }

    #[test]
    fn parse_callback_reports_codes_and_errors() {
        assert_eq!(
            parse_callback("GET /cb?code=x&state=s HTTP/1.1\r\nHost: h\r\n\r\n").unwrap(),
            ("x".to_string(), "s".to_string())
        );
        assert!(parse_callback("POST /cb?code=x&state=s HTTP/1.1").is_err());
        assert!(
            parse_callback("GET /cb?state=s HTTP/1.1").is_err(),
            "no code"
        );
        assert!(parse_callback("").is_err());
        // The error code is preserved with its description when one differs.
        assert_eq!(
            parse_callback(
                "GET /cb?error=invalid_target&error_description=needs%20resource HTTP/1.1"
            )
            .err(),
            Some("authorization denied (invalid_target): needs resource".to_string())
        );
        assert_eq!(
            parse_callback("GET /cb?error=access_denied HTTP/1.1").err(),
            Some("authorization denied (access_denied)".to_string())
        );
    }

    #[test]
    fn authorize_url_carries_pkce_state_scope_resource() {
        let url = authorize_url(
            "http://127.0.0.1:1/auth",
            "cid",
            "http://127.0.0.1:2/cb",
            "mcp:read",
            "https://srv.example",
            "challenge-x",
            "state-y",
        );
        assert!(url.starts_with("http://127.0.0.1:1/auth?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("code_challenge=challenge-x"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state-y"));
        assert!(url.contains("scope=mcp%3Aread"), "{url}");
        assert!(url.contains("resource="), "{url}");
        // No scope/resource -> those params stay absent.
        let bare = authorize_url("http://127.0.0.1:1/auth", "cid", "cb", "", "", "c", "s");
        assert!(
            !bare.contains("scope=") && !bare.contains("resource="),
            "{bare}"
        );
    }

    #[test]
    fn token_from_response_rejects_and_keeps_old_refresh() {
        let v = serde_json::json!({"access_token": "a", "refresh_token": "r", "expires_in": 90});
        let tok = token_from_response(&v, "https://t", "cid", None, None).unwrap();
        assert_eq!(tok.access_token, "a");
        assert_eq!(tok.refresh_token.as_deref(), Some("r"));
        assert!(tok.expires_at.is_some());

        // No refresh in the response -> keep the stored one.
        let tok = token_from_response(
            &serde_json::json!({"access_token": "a"}),
            "https://t",
            "cid",
            None,
            Some("kept".into()),
        )
        .unwrap();
        assert_eq!(tok.refresh_token.as_deref(), Some("kept"));

        // Missing access_token is an error.
        assert!(
            token_from_response(&serde_json::json!({}), "https://t", "cid", None, None).is_err()
        );
    }
}
