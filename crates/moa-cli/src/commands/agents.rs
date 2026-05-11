//! CLI commands for agent templates and agent principals.

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use moa_orchestrator_client::{CreateAgentTemplateRequest, RegisterAgentRequest};
use std::path::PathBuf;
use uuid::Uuid;

/// Agent command arguments.
#[derive(Debug, Args)]
pub(crate) struct AgentsCommand {
    /// Agent action to run.
    #[command(subcommand)]
    pub(crate) action: AgentsAction,
}

/// Agent lifecycle actions.
#[derive(Debug, Subcommand)]
pub(crate) enum AgentsAction {
    /// Create an agent template.
    CreateTemplate {
        /// Tenant-unique template name.
        #[arg(long)]
        name: String,
        /// Optional human-readable description.
        #[arg(long)]
        description: Option<String>,
        /// Path to the template instruction file.
        #[arg(long, value_name = "FILE")]
        instructions: PathBuf,
        /// Tool name allowed by this template.
        #[arg(long = "tool")]
        tools: Vec<String>,
    },
    /// List active agent templates.
    ListTemplates,
    /// Register an agent from a template.
    Register {
        /// Template UUID.
        #[arg(long)]
        template: Uuid,
        /// Human-readable agent name.
        #[arg(long)]
        name: String,
    },
    /// List active agents operated by the caller.
    List,
    /// Deactivate an agent.
    Deactivate {
        /// Agent UUID.
        id: Uuid,
    },
    /// Grant an agent the right to act as a user.
    GrantActAs {
        /// Agent UUID.
        #[arg(long)]
        agent: Uuid,
        /// User UUID.
        #[arg(long)]
        user: Uuid,
    },
    /// Revoke an agent's right to act as a user.
    RevokeActAs {
        /// Agent UUID.
        #[arg(long)]
        agent: Uuid,
        /// User UUID.
        #[arg(long)]
        user: Uuid,
    },
}

/// Run an agent command.
pub(crate) async fn handle_agents_command(command: AgentsCommand) -> Result<String> {
    let client = crate::client::client_from_credentials().await?;
    match command.action {
        AgentsAction::CreateTemplate {
            name,
            description,
            instructions,
            tools,
        } => {
            let instructions_text = tokio::fs::read_to_string(&instructions)
                .await
                .with_context(|| format!("read instructions {}", instructions.display()))?;
            let template = client
                .agent_templates_create(CreateAgentTemplateRequest {
                    name,
                    description,
                    instructions: instructions_text,
                    allowed_tools: tools,
                })
                .await?;
            Ok(format!(
                "Created agent template {}\nName: {}\n",
                template.id, template.name
            ))
        }
        AgentsAction::ListTemplates => {
            let templates = client.agent_templates_list().await?;
            if templates.is_empty() {
                return Ok("No agent templates.\n".to_string());
            }
            let mut output = format!("{:<38} {:<22} {}\n", "ID", "TOOLS", "NAME");
            for template in templates {
                output.push_str(&format!(
                    "{:<38} {:<22} {}\n",
                    template.id,
                    template.allowed_tools.join(","),
                    template.name
                ));
            }
            Ok(output)
        }
        AgentsAction::Register { template, name } => {
            let agent = client
                .agents_register(RegisterAgentRequest {
                    template_id: template,
                    display_name: name,
                })
                .await?;
            Ok(format!(
                "Registered agent {}\nName: {}\nStatus: {}\n",
                agent.id, agent.display_name, agent.status
            ))
        }
        AgentsAction::List => {
            let agents = client.agents_list().await?;
            if agents.is_empty() {
                return Ok("No agents.\n".to_string());
            }
            let mut output = format!("{:<38} {:<12} {}\n", "ID", "STATUS", "NAME");
            for agent in agents {
                output.push_str(&format!(
                    "{:<38} {:<12} {}\n",
                    agent.id, agent.status, agent.display_name
                ));
            }
            Ok(output)
        }
        AgentsAction::Deactivate { id } => {
            client.agents_deactivate(id).await?;
            Ok(format!("Deactivated agent {id}.\n"))
        }
        AgentsAction::GrantActAs { agent, user } => {
            client.agents_grant_can_act_as(agent, user).await?;
            Ok(format!("Granted agent {agent} can_act_as user {user}.\n"))
        }
        AgentsAction::RevokeActAs { agent, user } => {
            client.agents_revoke_can_act_as(agent, user).await?;
            Ok(format!("Revoked agent {agent} can_act_as user {user}.\n"))
        }
    }
}
