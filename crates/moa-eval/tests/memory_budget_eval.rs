include!("memory_eval_support/common.rs");

#[test]
fn budget_gate_zero_leak_fixture_passes_with_previous_report() -> TestResult {
    // Pins: the memory_retrieval budget gate accepts zero hard leaks and loads the previous report env path.
    let temp = tempfile::tempdir()?;
    let report_path = temp.path().join("current.json");
    let previous_path = temp.path().join("previous.json");
    let report = memory_budget_report(memory_budget_probe_results(false));
    write_memory_budget_report(&report_path, &report)?;
    write_memory_budget_report(&previous_path, &report)?;

    let output = run_memory_budget_gate(&report_path, Some(&previous_path))?;
    assert!(
        output.status.success(),
        "zero-leak memory budget fixture should pass:\n{}",
        command_output_text(&output)
    );
    let text = command_output_text(&output);
    assert!(
        text.contains("Memory-retrieval budgets passed")
            && text.contains("1 regression baseline(s) compared"),
        "pass output should mention the previous-report comparison:\n{text}"
    );

    Ok(())
}

#[test]
fn budget_gate_cross_user_leak_fixture_fails_with_probe_ids() -> TestResult {
    // Pins: a cross-user isolation leak is a hard budget failure with metric, expected/actual values, and probe id.
    let temp = tempfile::tempdir()?;
    let report_path = temp.path().join("cross-user-leak.json");
    write_memory_budget_report(
        &report_path,
        &memory_budget_report(memory_budget_probe_results(true)),
    )?;

    let output = run_memory_budget_gate(&report_path, None)?;
    assert!(
        !output.status.success(),
        "cross-user leak fixture should fail:\n{}",
        command_output_text(&output)
    );
    let text = command_output_text(&output);
    for expected in [
        "cross_user_leak_count",
        "expected 0",
        "actual 1",
        "affected probe IDs: probe-cross-user-leak",
    ] {
        assert!(
            text.contains(expected),
            "failure output should include `{expected}`:\n{text}"
        );
    }

    Ok(())
}

#[test]
fn budget_gate_previous_report_regression_fails_recall_mrr_ndcg_gate() -> TestResult {
    // Pins: previous memory reports from MOA_EVAL_PREVIOUS_MEMORY_REPORT gate recall, MRR, and nDCG regressions.
    let temp = tempfile::tempdir()?;
    let report_path = temp.path().join("current-regressed.json");
    let previous_path = temp.path().join("previous-strong.json");
    write_memory_budget_report(
        &report_path,
        &memory_budget_report(memory_budget_regression_probe_results(false)),
    )?;
    write_memory_budget_report(
        &previous_path,
        &memory_budget_report(memory_budget_regression_probe_results(true)),
    )?;

    let output = run_memory_budget_gate(&report_path, Some(&previous_path))?;
    assert!(
        !output.status.success(),
        "regressed memory budget fixture should fail:\n{}",
        command_output_text(&output)
    );
    let text = command_output_text(&output);
    for expected in [
        "retrieval.recall_at_4",
        "retrieval.mrr",
        "retrieval.ndcg_at_4",
        "expected regression <= 5.00%",
    ] {
        assert!(
            text.contains(expected),
            "regression output should include `{expected}`:\n{text}"
        );
    }

    Ok(())
}

#[test]
fn budget_gate_reranker_recall_regression_fails() -> TestResult {
    // Pins: reranker-on reports fail when post-rerank recall@4 regresses by more than three points.
    let temp = tempfile::tempdir()?;
    let report_path = temp.path().join("reranker-recall-regressed.json");
    write_memory_budget_report(
        &report_path,
        &memory_budget_report_with_reranker(reranker_recall_regression_probe_results(), true),
    )?;

    let output = run_memory_budget_gate(&report_path, None)?;
    assert!(
        !output.status.success(),
        "reranker recall regression fixture should fail:\n{}",
        command_output_text(&output)
    );
    let text = command_output_text(&output);
    for expected in [
        "retrieval.reranker_recall_at_4_regression",
        "pre 1.0000",
        "post 0.0000",
    ] {
        assert!(
            text.contains(expected),
            "reranker recall output should include `{expected}`:\n{text}"
        );
    }

    Ok(())
}

#[test]
fn budget_gate_reranker_latency_without_recall_gain_fails() -> TestResult {
    // Pins: reranker-on reports fail when p95 latency exceeds 2s without at least a three-point recall@4 gain.
    let temp = tempfile::tempdir()?;
    let report_path = temp.path().join("reranker-latency-regressed.json");
    write_memory_budget_report(
        &report_path,
        &memory_budget_report_with_reranker(reranker_latency_without_gain_probe_results(), true),
    )?;

    let output = run_memory_budget_gate(&report_path, None)?;
    assert!(
        !output.status.success(),
        "reranker latency fixture should fail:\n{}",
        command_output_text(&output)
    );
    let text = command_output_text(&output);
    for expected in [
        "retrieval.p95_retrieval_latency_ms",
        "expected <= 2000 unless recall@4 gain >= 0.03",
        "actual 2501",
    ] {
        assert!(
            text.contains(expected),
            "reranker latency output should include `{expected}`:\n{text}"
        );
    }

    Ok(())
}
