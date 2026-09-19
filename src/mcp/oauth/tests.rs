//! Tests, split out of the module body so it stays implementation.

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
    let err = parse_callback("GET /callback?error=invalid_target&error_description=nope HTTP/1.1")
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
