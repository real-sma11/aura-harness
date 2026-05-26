//! Aura CLI entry point.
//!
//! By default, starts the simple IRC-style terminal UI. Use `run --ui none`
//! to start in headless/swarm mode. Subcommands `login`, `logout`, and
//! `whoami` manage zOS authentication for proxy mode.

mod api_server;
mod cli;
mod event_loop;
mod record_loader;
mod session_helpers;

use cli::{Cli, Commands, RunArgs, UiMode};

use anyhow::Context;
use aura_agent::{
    AgentLoop, KernelModelGateway, KernelToolGateway, ProcessManager, ProcessManagerConfig,
};
use aura_core::{Identity, Transaction};
use aura_kernel::{Kernel, KernelConfig};
use aura_reasoner::ModelProvider;
use aura_terminal::{App, Terminal, Theme, UiCommand, UiEvent};
use aura_tools::ToolConfig;
use clap::Parser;
use colored::Colorize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

// ============================================================================
// Hello World (Spec 01)
// ============================================================================

pub fn hello_world_message() -> &'static str {
    "Hello, World!"
}

pub fn hello_world() {
    println!("{}", hello_world_message());
}

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Hello) => {
            hello_world();
            Ok(())
        }
        Some(Commands::Login) => cmd_login().await,
        Some(Commands::Logout) => cmd_logout().await,
        Some(Commands::Whoami) => cmd_whoami(),
        Some(Commands::Run(args)) => run_with_args(args).await,
        None => run_with_args(RunArgs::default()).await,
    }
}

async fn run_with_args(args: RunArgs) -> anyhow::Result<()> {
    match args.ui {
        UiMode::Terminal => run_terminal(args).await,
        UiMode::None => {
            let filter = if args.verbose {
                EnvFilter::from_default_env().add_directive("aura=debug".parse()?)
            } else {
                EnvFilter::from_default_env().add_directive("aura=info".parse()?)
            };

            tracing_subscriber::registry()
                .with(
                    fmt::layer()
                        .event_format(aura_runtime::console_format::AuraConsoleFormat::new()),
                )
                .with(filter)
                .init();

            run_headless(args).await
        }
    }
}

// ============================================================================
// Auth Commands
// ============================================================================

async fn cmd_login() -> anyhow::Result<()> {
    use std::io::Write;

    print!("Email: ");
    std::io::stdout().flush()?;

    let mut email = String::new();
    std::io::stdin().read_line(&mut email)?;
    let email = email.trim();

    if email.is_empty() {
        anyhow::bail!("Email cannot be empty");
    }

    let password = rpassword::prompt_password_stdout("Password: ")?;
    if password.is_empty() {
        anyhow::bail!("Password cannot be empty");
    }

    println!("{} Authenticating...", "▶".blue().bold());

    let client = aura_auth::ZosClient::new()?;
    let session = client.login(email, &password).await?;

    let display = session.display_name.clone();
    let zid = session.primary_zid.clone();

    aura_auth::CredentialStore::save(&session)?;

    println!(
        "{} Logged in as {} ({})",
        "✓".green().bold(),
        display.green(),
        zid,
    );

    Ok(())
}

async fn cmd_logout() -> anyhow::Result<()> {
    if let Some(stored) = aura_auth::CredentialStore::load() {
        let client = aura_auth::ZosClient::new()?;
        client.logout(&stored.access_token).await;
    }

    aura_auth::CredentialStore::clear()?;
    println!("{} Logged out", "✓".green().bold());
    Ok(())
}

fn cmd_whoami() -> anyhow::Result<()> {
    match aura_auth::CredentialStore::load() {
        Some(session) => {
            println!("{}", "Authentication".cyan().bold());
            println!("  Name:    {}", session.display_name);
            println!("  zID:     {}", session.primary_zid);
            println!("  User ID: {}", session.user_id);
            println!(
                "  Since:   {}",
                session.created_at.format("%Y-%m-%d %H:%M UTC")
            );
        }
        None => {
            println!(
                "{} Not logged in. Run `aura login` to authenticate.",
                "ℹ".blue().bold()
            );
        }
    }
    Ok(())
}

// ============================================================================
// Terminal Mode
// ============================================================================

async fn run_terminal(args: RunArgs) -> anyhow::Result<()> {
    let theme = Theme::by_name(&args.theme);

    let (ui_tx, mut ui_rx) = mpsc::channel::<UiEvent>(100);
    let (cmd_tx, cmd_rx) = mpsc::channel::<UiCommand>(200);

    let mut app = App::new()
        .with_event_sender(ui_tx.clone())
        .with_command_receiver(cmd_rx);

    if args.verbose {
        app.set_verbose(true);
    }

    let mut terminal = Terminal::new(theme)?;

    let data_dir = args
        .dir
        .clone()
        .or_else(|| std::env::var("AURA_DATA_DIR").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("./aura_data"));

    let workspace_root = data_dir.join("workspaces");
    tokio::fs::create_dir_all(&data_dir).await?;
    tokio::fs::create_dir_all(&workspace_root).await?;

    let identity_file = data_dir.join("agent_identity.txt");
    let zns_id = if identity_file.exists() {
        tokio::fs::read_to_string(&identity_file).await?
    } else {
        let new_id = format!("0://terminal/{}", uuid::Uuid::new_v4());
        tokio::fs::write(&identity_file, &new_id).await?;
        new_id
    };
    let identity = Identity::new(&zns_id, "Terminal Agent");

    let store_path = session_helpers::resolve_store_path(&data_dir);
    let store = session_helpers::open_store(&store_path)?;

    record_loader::load_existing_records(&store, identity.agent_id, &cmd_tx);
    record_loader::send_initial_agent(&identity, &store, &cmd_tx);
    api_server::start_api_server(cmd_tx.clone(), workspace_root.clone()).await;

    let mut tool_config = ToolConfig::for_autonomous_dev_loop();
    if args.allow_unrestricted_full_access || unrestricted_full_access_from_env() {
        tool_config.command.allow_unrestricted_full_access = true;
        warn!("unrestricted full-access command allowlist bypass enabled by operator config");
    }
    if tool_config.command.enabled {
        info!(
            binary_allowlist = ?tool_config.command.binary_allowlist,
            allow_shell = tool_config.command.allow_shell,
            allow_unrestricted_full_access = tool_config.command.allow_unrestricted_full_access,
            "run_command enabled"
        );
    }
    let (executor_router, tools) = session_helpers::build_executor_router_with_config(&tool_config);

    let config = session_helpers::default_agent_config();
    let agent_loop = AgentLoop::new(config);

    let (process_tx, process_rx) = mpsc::channel::<Transaction>(100);
    let process_manager = Arc::new(ProcessManager::new(
        process_tx,
        ProcessManagerConfig::default(),
    ));

    // After the proxy-only collapse there are exactly two providers:
    // `mock` for explicit test / offline mode, and the router-backed
    // Anthropic-shaped client for everything else. The legacy
    // `--provider` arg only ever takes `"anthropic"` or `"mock"`.
    let selection = match args.provider.as_str() {
        "mock" => Ok(aura_reasoner::mock_provider()),
        _ => aura_reasoner::default_provider_from_env(),
    }
    .context("building model provider")?;
    if selection.name != "anthropic" {
        let _ = cmd_tx.try_send(UiCommand::SetStatus("Mock Mode".to_string()));
    }
    let provider: Arc<dyn ModelProvider + Send + Sync> = selection.provider;

    let kernel_config = KernelConfig {
        workspace_base: workspace_root.clone(),
        policy: aura_kernel::PolicyConfig::default(),
        ..KernelConfig::default()
    };
    let agent_id = identity.agent_id;
    let kernel = Arc::new(Kernel::new(
        store.clone() as Arc<dyn aura_store::Store>,
        provider,
        executor_router,
        kernel_config,
        agent_id,
    )?);

    let model_gateway = KernelModelGateway::new(kernel.clone());
    let tool_gateway = KernelToolGateway::new(kernel.clone());

    let cmd_tx_clone = cmd_tx.clone();
    let process_manager_clone = Arc::clone(&process_manager);

    let processor_handle = tokio::spawn(async move {
        let mut agent_loop = agent_loop;
        let ctx = event_loop::EventLoopContext {
            events: &mut ui_rx,
            process_completions: process_rx,
            commands: cmd_tx_clone,
            agent_loop: &mut agent_loop,
            model_gateway: &model_gateway,
            tool_gateway: &tool_gateway,
            tools: &tools,
            kernel,
            agent_id,
            _process_manager: process_manager_clone,
            memory_manager: None,
        };
        event_loop::run_event_loop(ctx).await
    });

    terminal.run(&mut app)?;

    // Phase 5 (error-handling polish): we used to call
    // `processor_handle.abort()` and return `Ok(())` unconditionally,
    // which masked any panic / structured error inside the agent
    // loop. Surface the failure when the loop has already terminated
    // (so the CLI exits non-zero), and only fall back to `abort` for
    // the ordinary case where the user quit the UI while the loop
    // was still running.
    if processor_handle.is_finished() {
        match processor_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e.context("agent event loop failed")),
            Err(join_err) if join_err.is_cancelled() => {}
            Err(join_err) => {
                return Err(anyhow::anyhow!(
                    "agent event loop task did not join cleanly: {join_err}"
                ));
            }
        }
    } else {
        processor_handle.abort();
    }

    Ok(())
}

// ============================================================================
// Headless Mode (Node)
// ============================================================================

async fn run_headless(args: RunArgs) -> anyhow::Result<()> {
    info!("Starting AURA CLI in headless mode (node server)");

    let mut config = aura_runtime::NodeConfig::from_env();
    config.allow_unrestricted_full_access |= args.allow_unrestricted_full_access;

    aura_runtime::Node::new(config).run().await
}

fn unrestricted_full_access_from_env() -> bool {
    std::env::var("AURA_ALLOW_UNRESTRICTED_FULL_ACCESS")
        .ok()
        .map(|value| {
            let value = value.trim();
            value == "1" || value.eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hello_world_message_exact() {
        assert_eq!(hello_world_message(), "Hello, World!");
    }

    #[test]
    fn test_hello_world_prints() {
        hello_world();
    }
}
