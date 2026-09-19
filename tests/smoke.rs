use dex::cli::{parse_args, resolve_mode};

#[test]
fn cli_parses_and_resolves() {
    let args = parse_args();
    let _ = resolve_mode(&args);
}
