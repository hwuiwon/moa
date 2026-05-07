//! Eval command handlers.

use super::*;

pub(crate) async fn handle_eval_run(args: EvalRunArgs, config: MoaConfig) -> Result<i32> {
    let suite = load_suite(&args.suite).context("loading eval suite")?;
    let configs = load_eval_configs(&args.config)?;
    let evaluators = build_evaluators(
        &args.evaluator,
        &EvaluatorOptions {
            max_cost_dollars: args.max_cost,
            max_latency_ms: args.max_latency,
            max_tokens: args.max_tokens,
            max_tool_calls: args.max_tool_calls,
            max_turns: args.max_turns,
        },
    )
    .context("building evaluators")?;
    let reporters = build_reporters(
        &args.report,
        &ReporterOptions {
            verbose: args.verbose,
            color: !args.ci && std::io::stdout().is_terminal(),
            json_pretty: true,
        },
    )
    .context("building reporters")?;

    let engine = EvalEngine::new(
        config,
        EngineOptions {
            parallel: args.parallel,
            ..EngineOptions::default()
        },
    )
    .context("creating eval engine")?;

    let mut run = engine
        .run_suite(&suite, &configs)
        .await
        .context("running eval suite")?;
    evaluate_run(&suite, &mut run, &evaluators)
        .await
        .context("scoring eval results")?;

    for reporter in &reporters {
        reporter
            .report(&suite, &configs, &run)
            .await
            .context("reporting eval results")?;
    }

    Ok(eval_exit_code(args.ci, &run))
}

pub(crate) fn handle_eval_plan(args: EvalPlanArgs, config: MoaConfig) -> Result<()> {
    let suite = load_suite(&args.suite).context("loading eval suite")?;
    let configs = load_eval_configs(&args.config)?;
    let engine =
        EvalEngine::new(config, EngineOptions::default()).context("creating eval engine")?;
    let plan = engine.plan(&suite, &configs);

    println!("Suite: {}", plan.suite_name);
    println!("Configs: {}", plan.configs.join(", "));
    println!("Cases: {}", plan.cases.join(", "));
    println!("Total runs: {}", plan.total_runs);
    println!(
        "Estimated cost: ${:.4} - ${:.4}",
        plan.estimated_cost_range.0, plan.estimated_cost_range.1
    );
    Ok(())
}

pub(crate) async fn handle_eval_skill(args: EvalSkillArgs, config: MoaConfig) -> Result<i32> {
    let _graph_store = Arc::new(load_graph_store(&config).await?);
    let _ = args;
    bail!("moa eval skill is pending the C04 graph-native skill regression migration");
}

pub(crate) fn handle_eval_list(dir: PathBuf) -> Result<()> {
    let paths = discover_suites(&dir).context("discovering eval suites")?;
    for path in paths {
        let suite =
            load_suite(&path).with_context(|| format!("loading suite from {}", path.display()))?;
        println!(
            "{:30} | {:3} cases | {}",
            suite.name,
            suite.cases.len(),
            suite.description.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

pub(crate) async fn handle_eval_datasets(
    config: &MoaConfig,
    command: EvalDatasetsCommand,
) -> Result<String> {
    let store = load_session_store(config).await?;
    match command {
        EvalDatasetsCommand::Register(args) => {
            let dataset_id = register_dataset(store.pool(), &args.path, &args.name)
                .await
                .context("registering eval dataset")?;
            Ok(format!(
                "dataset: {dataset_id}\nname: {}\npath: {}\n",
                args.name,
                args.path.display()
            ))
        }
        EvalDatasetsCommand::List => {
            let rows = list_datasets(store.pool())
                .await
                .context("listing eval datasets")?;
            let mut report = String::from("dataset_id\tname\titems\n");
            for (dataset_id, name, items) in rows {
                report.push_str(&format!("{dataset_id}\t{name}\t{items}\n"));
            }
            Ok(report)
        }
    }
}

pub(crate) async fn handle_eval_replay(config: &MoaConfig, args: EvalReplayArgs) -> Result<String> {
    let store = load_session_store(config).await?;
    let (sink, writer) = MpscSink::spawn(
        MpscSinkConfig::from(&config.observability.lineage),
        store.pool().clone(),
    )
    .await
    .context("starting lineage writer for eval replay")?;
    let run_id = args.run_id.unwrap_or_else(Uuid::now_v7);
    let report = replay_dataset_live(
        config.clone(),
        store.pool(),
        Arc::new(sink) as Arc<dyn moa_lineage_core::LineageSink>,
        moa_eval::ReplayConfig {
            dataset_id: args.dataset,
            run_id,
            model_override: args.model,
            embedder_override: args.embedder,
            limit: args.limit,
        },
    )
    .await
    .context("running eval replay")?;
    writer
        .shutdown()
        .await
        .context("flushing eval replay scores")?;

    Ok(format!(
        "run_id: {}\ndataset_id: {}\nitems: {}\nscores: {}\n",
        report.run_id, report.dataset_id, report.items, report.scores
    ))
}

pub(crate) async fn handle_eval_scores(config: &MoaConfig, args: EvalScoresArgs) -> Result<String> {
    let store = load_session_store(config).await?;
    let rows = sqlx::query(
        r#"
        SELECT name,
               value_type,
               COUNT(*)::BIGINT AS n,
               AVG(value_numeric) AS numeric_mean,
               AVG(CASE WHEN value_boolean THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION AS boolean_rate
        FROM analytics.scores
        WHERE run_id = $1
        GROUP BY name, value_type
        ORDER BY name, value_type
        "#,
    )
    .bind(args.run_id)
    .fetch_all(store.pool())
    .await?;

    let mut report = format!("run_id: {}\nname\ttype\tn\tmean_or_rate\n", args.run_id);
    for row in rows {
        let name: String = row.try_get("name")?;
        let value_type: String = row.try_get("value_type")?;
        let n: i64 = row.try_get("n")?;
        let numeric_mean: Option<f64> = row.try_get("numeric_mean")?;
        let boolean_rate: Option<f64> = row.try_get("boolean_rate")?;
        let value = numeric_mean.or(boolean_rate).unwrap_or(0.0);
        report.push_str(&format!("{name}\t{value_type}\t{n}\t{value:.4}\n"));
    }
    Ok(report)
}

pub(crate) async fn handle_eval_compare(
    config: &MoaConfig,
    args: EvalCompareArgs,
) -> Result<String> {
    let store = load_session_store(config).await?;
    let rows = sqlx::query(
        r#"
        WITH base AS (
            SELECT name, AVG(value_numeric) AS mean
            FROM analytics.scores
            WHERE run_id = $1 AND value_type = 'numeric'
            GROUP BY name
        ),
        new AS (
            SELECT name, AVG(value_numeric) AS mean
            FROM analytics.scores
            WHERE run_id = $2 AND value_type = 'numeric'
            GROUP BY name
        )
        SELECT COALESCE(base.name, new.name) AS name,
               base.mean AS base_mean,
               new.mean AS new_mean,
               COALESCE(new.mean, 0.0) - COALESCE(base.mean, 0.0) AS delta
        FROM base
        FULL OUTER JOIN new USING (name)
        ORDER BY name
        "#,
    )
    .bind(args.base_run)
    .bind(args.new_run)
    .fetch_all(store.pool())
    .await?;

    let mut report = format!(
        "base_run: {}\nnew_run: {}\nname\tbase\tnew\tdelta\n",
        args.base_run, args.new_run
    );
    for row in rows {
        let name: String = row.try_get("name")?;
        let base_mean: Option<f64> = row.try_get("base_mean")?;
        let new_mean: Option<f64> = row.try_get("new_mean")?;
        let delta: f64 = row.try_get("delta")?;
        report.push_str(&format!(
            "{name}\t{}\t{}\t{delta:.4}\n",
            format_optional_f64(base_mean),
            format_optional_f64(new_mean)
        ));
    }
    Ok(report)
}

pub(crate) fn format_optional_f64(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.4}"))
        .unwrap_or_else(|| "-".to_string())
}

pub(crate) fn load_eval_configs(paths: &[PathBuf]) -> Result<Vec<AgentConfig>> {
    paths
        .iter()
        .map(|path| {
            load_agent_config(path)
                .with_context(|| format!("loading config from {}", path.display()))
        })
        .collect()
}

pub(crate) fn eval_exit_code(ci: bool, run: &EvalRun) -> i32 {
    if !ci {
        return 0;
    }
    if run
        .results
        .iter()
        .any(|result| matches!(result.status, EvalStatus::Error | EvalStatus::Timeout))
    {
        return 2;
    }
    if run
        .results
        .iter()
        .any(|result| matches!(result.status, EvalStatus::Failed))
    {
        return 1;
    }
    0
}
