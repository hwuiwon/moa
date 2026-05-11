//! Top-level CLI command dispatch.

use super::*;

/// Dispatches one parsed CLI command.
pub(crate) async fn dispatch(cli: Cli, config: MoaConfig) -> Result<()> {
    match cli.command {
        None => {
            if let Some(prompt) = cli.prompt {
                exec::run_exec(config, prompt).await?;
            } else {
                let mut command = Cli::command();
                command.print_long_help()?;
                println!();
            }
        }
        Some(CommandKind::Exec(args)) => {
            exec::run_exec(config, args.prompt).await?;
        }
        Some(CommandKind::Status) => {
            print!("{}", status_report(&config).await?);
        }
        Some(CommandKind::Sessions(args)) => {
            print!(
                "{}",
                sessions_report(&config, args.workspace.as_deref()).await?
            );
        }
        Some(CommandKind::Session { command }) => match command {
            SessionCommand::Stats { id } => {
                print!("{}", session_stats_report(&config, &id).await?);
            }
        },
        Some(CommandKind::Workspace { command }) => match command {
            WorkspaceCommand::Stats(args) => {
                print!(
                    "{}",
                    workspace_stats_report(&config, args.workspace.as_deref(), args.days).await?
                );
            }
        },
        Some(CommandKind::Tool { command }) => match command {
            ToolCommand::Stats(args) => {
                print!(
                    "{}",
                    tool_stats_report(&config, args.workspace.as_deref()).await?
                );
            }
        },
        Some(CommandKind::Cache { command }) => match command {
            CacheCommand::Stats(args) => {
                print!(
                    "{}",
                    cache_stats_report(&config, args.workspace.as_deref(), args.days).await?
                );
            }
        },
        Some(CommandKind::Memory { command }) => match command {
            MemoryCommand::Search { query, limit } => {
                print!("{}", memory_search_report(&config, &query, limit).await?);
            }
            MemoryCommand::Show { uid } => {
                print!("{}", memory_show_report(&config, &uid).await?);
            }
            MemoryCommand::Ingest(args) => {
                print!(
                    "{}",
                    memory_ingest_report(
                        &config,
                        &args.files,
                        args.name.as_deref(),
                        args.workspace.as_deref(),
                    )
                    .await?
                );
            }
        },
        Some(CommandKind::Explain { id }) => {
            print!("{}", explain_report(&config, &id).await?);
        }
        Some(CommandKind::Lineage { command }) => match command {
            LineageCommand::Query(args) => {
                print!("{}", lineage_query_report(&config, &args).await?);
            }
            LineageCommand::Export(args) => {
                print!("{}", lineage_export_report(&config, &args).await?);
            }
            LineageCommand::Verify(args) => {
                print!("{}", lineage_verify_report(&config, &args).await?);
            }
            LineageCommand::Erase(args) => {
                print!("{}", lineage_erase_report(&config, &args).await?);
            }
        },
        Some(CommandKind::Retrieve(args)) => {
            print!("{}", retrieve_report(&config, &args).await?);
        }
        Some(CommandKind::Skills { command }) => {
            print!("{}", handle_skills_command(&config, command).await?);
        }
        Some(CommandKind::Privacy { command }) => {
            print!("{}", handle_privacy_command(&config, command).await?);
        }
        Some(CommandKind::Auth(command)) => {
            print!("{}", handle_auth_command(command).await?);
        }
        Some(CommandKind::Approvals(command)) => {
            print!("{}", handle_approvals_command(command).await?);
        }
        Some(CommandKind::PromoteWorkspace(args)) => {
            print!(
                "{}",
                handle_admin_command(&config, AdminCommand::PromoteWorkspace(args)).await?
            );
        }
        Some(CommandKind::RollbackPromotion(args)) => {
            print!(
                "{}",
                handle_admin_command(&config, AdminCommand::RollbackPromotion(args)).await?
            );
        }
        Some(CommandKind::FinalizePromotion(args)) => {
            print!(
                "{}",
                handle_admin_command(&config, AdminCommand::FinalizePromotion(args)).await?
            );
        }
        Some(CommandKind::Config { command }) => match command {
            None => {
                let rendered = toml::to_string_pretty(&config).context("serializing config")?;
                print!("{rendered}");
            }
            Some(ConfigCommand::Set { key, value }) => {
                let mut updated = config;
                apply_config_update(&mut updated, &key, &value)?;
                updated.save_async().await?;
                print!("{}", toml::to_string_pretty(&updated)?);
            }
        },
        Some(CommandKind::Init) => {
            init_workspace(&config).await?;
            println!("initialized MOA workspace for {}", current_workspace_id());
        }
        Some(CommandKind::Version) => {
            println!("{}", version_text());
        }
        Some(CommandKind::Doctor) => {
            let log_path = cli.log_file.clone().unwrap_or_else(default_log_path);
            print!("{}", doctor_report(&config, &log_path).await?);
        }
        Some(CommandKind::Daemon { command }) => match command {
            DaemonCommand::Status => print!("{}", daemon_status_report(&config).await?),
        },
        Some(CommandKind::Checkpoint { command }) => match command {
            CheckpointCommand::Create { label } => {
                print!("{}", checkpoint_create_report(&config, &label).await?);
            }
            CheckpointCommand::List => {
                print!("{}", checkpoint_list_report(&config).await?);
            }
            CheckpointCommand::Rollback { id } => {
                print!("{}", checkpoint_rollback_report(config, &id).await?);
            }
            CheckpointCommand::Cleanup => {
                print!("{}", checkpoint_cleanup_report(&config).await?);
            }
        },
        Some(CommandKind::Eval { command }) => match command {
            EvalCommand::Run(args) => {
                let exit_code = handle_eval_run(args, config).await?;
                if exit_code != 0 {
                    std::process::exit(exit_code);
                }
            }
            EvalCommand::Plan(args) => {
                handle_eval_plan(args, config)?;
            }
            EvalCommand::Datasets { command } => {
                print!("{}", handle_eval_datasets(&config, command).await?);
            }
            EvalCommand::Replay(args) => {
                print!("{}", handle_eval_replay(&config, args).await?);
            }
            EvalCommand::Scores(args) => {
                print!("{}", handle_eval_scores(&config, args).await?);
            }
            EvalCommand::Compare(args) => {
                print!("{}", handle_eval_compare(&config, args).await?);
            }
            EvalCommand::Skill(args) => {
                let exit_code = handle_eval_skill(args, config).await?;
                if exit_code != 0 {
                    std::process::exit(exit_code);
                }
            }
            EvalCommand::List { dir } => {
                handle_eval_list(dir)?;
            }
        },
    }

    Ok(())
}
