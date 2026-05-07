//! Shell approval pattern regression tests.

use moa_security::parse_and_match_bash;

#[test]
fn shell_chain_after_matching_prefix_does_not_satisfy_simple_glob() {
    assert!(
        !parse_and_match_bash("npm test && rm -rf /", "npm test*"),
        "bash approval matching must reject chained commands when any segment fails the allow glob"
    );
    assert!(
        parse_and_match_bash("npm test -- --watch", "npm test*"),
        "the same glob should still allow a single matching npm test command"
    );
}
