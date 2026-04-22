mod client;
mod config;
mod host;
mod pty;
mod sdp;
mod serve;
mod session;
mod ten_kb_site;
mod terminal;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use client::ClientSession;
use config::{
    config_path, hash_password, init_config, load_or_create_config, read_serve_runtime, save_config,
    BoottyConfig, ServeAuthType, ServeMode,
};
use host::HostSession;
use rand::Rng;
use serve::{AdminCmdRequest, AdminCmdResponse, AdminSessionItem, ServeAuthRuntime, ServeLaunchOptions};
use std::env;
use std::io::{self, Write};
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "bootty")]
#[command(about = "Share terminal sessions over WebRTC")]
struct Cli {
    #[arg(short = 'v', long = "verbose", global = true, help = "Enable verbose logging")]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Host(HostArgs),
    Connect(ConnectArgs),
    Serve(ServeArgs),
    Ls(LsArgs),
    Cmd(CmdArgs),
    Config(ConfigArgs),
}

#[derive(Args, Debug)]
struct HostArgs {
    #[arg(long = "one-way", help = "Enable one-way connection flow")]
    one_way: bool,

    #[arg(long = "non-interactive", help = "Do not read from host stdin")]
    non_interactive: bool,

    #[arg(long = "stun", value_name = "URL", help = "Override STUN server (repeatable)")]
    stun_servers: Vec<String>,

    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "CMD",
        help = "Command to run in PTY (use after --)"
    )]
    cmd: Vec<String>,
}

#[derive(Args, Debug)]
struct ConnectArgs {
    #[arg(value_name = "OFFER", help = "Offer string from host")]
    offer: String,

    #[arg(long = "stun", value_name = "URL", help = "Override STUN server (repeatable)")]
    stun_servers: Vec<String>,
}

#[derive(Args, Debug)]
struct ServeArgs {
    #[arg(long = "host", value_name = "HOST", help = "Public bind host")]
    host: Option<String>,

    #[arg(long = "port", value_name = "PORT", help = "Public bind port")]
    port: Option<u16>,

    #[arg(long = "mode", value_name = "MODE", help = "Serve mode")]
    mode: Option<ServeModeArg>,

    #[arg(long = "auth", value_name = "AUTH", help = "Authentication method")]
    auth: Option<ServeAuthArg>,

    #[arg(long = "max-sessions", value_name = "N", help = "Maximum active sessions")]
    max_sessions: Option<usize>,

    #[arg(long = "stun", value_name = "URL", help = "Override STUN server (repeatable)")]
    stun_servers: Vec<String>,

    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "CMD",
        help = "Command to run in PTY (use after --)"
    )]
    cmd: Vec<String>,
}

#[derive(Args, Debug)]
struct LsArgs {
    #[arg(long = "json", help = "Print JSON output")]
    json: bool,

    #[arg(long = "state", value_name = "STATE", help = "Filter by session state")]
    state: Option<SessionStateArg>,
}

#[derive(Args, Debug)]
struct CmdArgs {
    #[arg(value_name = "SESSION_ID", help = "Target session id")]
    session_id: String,

    #[arg(long = "no-enter", help = "Do not append newline")]
    no_enter: bool,

    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        num_args = 1..,
        value_name = "CMD",
        help = "Command payload (use after --)"
    )]
    cmd: Vec<String>,
}

#[derive(Args, Debug)]
struct ConfigArgs {
    #[command(subcommand)]
    action: ConfigAction,
}

#[derive(Subcommand, Debug)]
enum ConfigAction {
    Path,
    List,
    Get { key: String },
    Set { key: String, value: String },
    Unset { key: String },
    Init {
        #[arg(long = "force", help = "Overwrite existing config")]
        force: bool,
    },
    SetPassword {
        #[arg(long = "prompt", help = "Prompt for password from stdin")]
        prompt: bool,
    },
}

#[derive(Debug, Clone, ValueEnum)]
enum ServeModeArg {
    Local,
    LanOpen,
    LanAuth,
}

#[derive(Debug, Clone, ValueEnum)]
enum ServeAuthArg {
    None,
    Pin,
    Password,
}

#[derive(Debug, Clone, ValueEnum)]
enum SessionStateArg {
    Pending,
    Connected,
}

impl SessionStateArg {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Connected => "connected",
        }
    }
}

impl From<ServeModeArg> for ServeMode {
    fn from(value: ServeModeArg) -> Self {
        match value {
            ServeModeArg::Local => ServeMode::Local,
            ServeModeArg::LanOpen => ServeMode::LanOpen,
            ServeModeArg::LanAuth => ServeMode::LanAuth,
        }
    }
}

impl From<ServeMode> for ServeModeArg {
    fn from(value: ServeMode) -> Self {
        match value {
            ServeMode::Local => ServeModeArg::Local,
            ServeMode::LanOpen => ServeModeArg::LanOpen,
            ServeMode::LanAuth => ServeModeArg::LanAuth,
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    if let Err(err) = run(cli).await {
        eprintln!("Fatal error: {err:#}");
        std::process::exit(1);
    }
}

fn init_logging(verbose: bool) {
    if !verbose {
        return;
    }

    let filter = env::var("RUST_LOG").unwrap_or_else(|_| {
        "info,bootty=trace,webrtc=info,webrtc_ice=warn,webrtc_sctp=warn,webrtc_mdns=warn,webrtc_dtls=warn".to_string()
    });

    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(EnvFilter::new(filter))
        .try_init();
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Host(args) => run_host(args).await,
        Commands::Connect(args) => run_connect(args).await,
        Commands::Serve(args) => run_serve(args).await,
        Commands::Ls(args) => run_ls(args).await,
        Commands::Cmd(args) => run_cmd(args).await,
        Commands::Config(args) => run_config(args),
    }
}

async fn run_host(args: HostArgs) -> Result<()> {
    let cfg = load_or_create_config()?;
    let stun_servers = resolve_stun_servers(&args.stun_servers, &cfg);
    let cmd = resolve_command(&args.cmd, &cfg);
    let non_interactive = args.non_interactive || cfg.host.non_interactive;

    let mut host = HostSession::new(cmd, non_interactive, args.one_way, stun_servers).await?;
    host.run().await
}

async fn run_connect(args: ConnectArgs) -> Result<()> {
    let cfg = load_or_create_config()?;
    let stun_servers = resolve_stun_servers(&args.stun_servers, &cfg);

    let mut client = ClientSession::new(args.offer, stun_servers).await?;
    client.run().await
}

async fn run_serve(args: ServeArgs) -> Result<()> {
    let cfg = load_or_create_config()?;

    let mode = args
        .mode
        .clone()
        .unwrap_or_else(|| cfg.serve.mode.clone().into());
    let host = resolve_serve_host(args.host.clone(), args.mode.as_ref(), &mode, &cfg);
    let port = args.port.unwrap_or(cfg.serve.port);
    let max_sessions = args.max_sessions.unwrap_or(cfg.serve.max_sessions);
    let stun_servers = resolve_stun_servers(&args.stun_servers, &cfg);
    let cmd = resolve_command(&args.cmd, &cfg);
    let auth = resolve_serve_auth(args.auth.clone(), &mode, &cfg)?;

    let options = ServeLaunchOptions {
        host,
        port,
        cmd,
        non_interactive: true,
        stun_servers,
        max_sessions,
        mode: mode.into(),
        auth,
    };

    serve::start_server(options).await
}

async fn run_ls(args: LsArgs) -> Result<()> {
    let runtime = read_serve_runtime().context("Failed to read active serve runtime")?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("Failed to create HTTP client")?;

    let mut request = client
        .get(format!("http://{}/admin/sessions", runtime.admin_addr))
        .header("x-bootty-admin-token", runtime.admin_token);

    if let Some(state) = args.state {
        request = request.query(&[("state", state.as_str())]);
    }

    let response = request.send().await.context("Failed to request session list")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("admin request failed: status={}, body={}", status, body);
    }

    let sessions: Vec<AdminSessionItem> = response
        .json()
        .await
        .context("Failed to parse session list response")?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
        return Ok(());
    }

    print_session_table(&sessions);
    Ok(())
}

async fn run_cmd(args: CmdArgs) -> Result<()> {
    let runtime = read_serve_runtime().context("Failed to read active serve runtime")?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("Failed to create HTTP client")?;

    let payload = AdminCmdRequest {
        cmd: args.cmd.join(" "),
        append_enter: !args.no_enter,
    };

    let response = client
        .post(format!(
            "http://{}/admin/sessions/{}/cmd",
            runtime.admin_addr, args.session_id
        ))
        .header("x-bootty-admin-token", runtime.admin_token)
        .json(&payload)
        .send()
        .await
        .context("Failed to send command injection request")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("admin request failed: status={}, body={}", status, body);
    }

    let body: AdminCmdResponse = response
        .json()
        .await
        .context("Failed to parse command injection response")?;

    println!(
        "Command injected: session_id={}, bytes_sent={}",
        body.session_id, body.bytes_sent
    );
    Ok(())
}

fn run_config(args: ConfigArgs) -> Result<()> {
    match args.action {
        ConfigAction::Path => {
            println!("{}", config_path()?.display());
            Ok(())
        }
        ConfigAction::List => {
            let cfg = load_or_create_config()?;
            println!("{}", serde_json::to_string_pretty(&cfg)?);
            Ok(())
        }
        ConfigAction::Get { key } => {
            let cfg = load_or_create_config()?;
            let value = get_config_value(&cfg, &key)?;
            println!("{value}");
            Ok(())
        }
        ConfigAction::Set { key, value } => {
            let mut cfg = load_or_create_config()?;
            set_config_value(&mut cfg, &key, &value)?;
            validate_config_edit(&cfg)?;
            save_config(&cfg)?;
            println!("Config updated: {}", key);
            Ok(())
        }
        ConfigAction::Unset { key } => {
            let mut cfg = load_or_create_config()?;
            unset_config_value(&mut cfg, &key)?;
            validate_config_edit(&cfg)?;
            save_config(&cfg)?;
            println!("Config key reset: {}", key);
            Ok(())
        }
        ConfigAction::Init { force } => {
            let cfg = init_config(force)?;
            println!("Config initialized: {}", config_path()?.display());
            println!("{}", serde_json::to_string_pretty(&cfg)?);
            Ok(())
        }
        ConfigAction::SetPassword { prompt } => {
            if !prompt {
                bail!("set-password requires --prompt");
            }
            let mut cfg = load_or_create_config()?;
            let password = prompt_password()?;
            let encoded = hash_password(&password)?;
            cfg.serve.auth.password_hash = Some(encoded);
            if cfg.serve.auth.auth_type == ServeAuthType::None {
                cfg.serve.auth.auth_type = ServeAuthType::Password;
            }
            save_config(&cfg)?;
            println!("Password hash updated in config");
            Ok(())
        }
    }
}

fn prompt_password() -> Result<String> {
    print!("Enter password: ");
    io::stdout().flush().context("Failed to flush stdout")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("Failed to read password")?;
    let password = input.trim().to_string();
    if password.is_empty() {
        bail!("Password must not be empty");
    }
    Ok(password)
}

fn resolve_stun_servers(cli_values: &[String], cfg: &BoottyConfig) -> Vec<String> {
    if cli_values.is_empty() {
        cfg.network.stun_servers.clone()
    } else {
        cli_values.to_vec()
    }
}

fn resolve_command(cli_cmd: &[String], cfg: &BoottyConfig) -> Vec<String> {
    if cli_cmd.is_empty() {
        cfg.host.default_cmd.clone()
    } else {
        cli_cmd.to_vec()
    }
}

fn resolve_serve_host(
    cli_host: Option<String>,
    raw_mode_arg: Option<&ServeModeArg>,
    mode: &ServeModeArg,
    cfg: &BoottyConfig,
) -> String {
    if let Some(host) = cli_host {
        return host;
    }

    if raw_mode_arg.is_some() {
        return mode_default_host(mode).to_string();
    }

    cfg.serve.host.clone()
}

fn mode_default_host(mode: &ServeModeArg) -> &'static str {
    match mode {
        ServeModeArg::Local => "127.0.0.1",
        ServeModeArg::LanOpen | ServeModeArg::LanAuth => "0.0.0.0",
    }
}

fn resolve_serve_auth(
    cli_auth: Option<ServeAuthArg>,
    mode: &ServeModeArg,
    cfg: &BoottyConfig,
) -> Result<ServeAuthRuntime> {
    let auth = cli_auth.unwrap_or_else(|| match cfg.serve.auth.auth_type {
        ServeAuthType::None => ServeAuthArg::None,
        ServeAuthType::Pin => ServeAuthArg::Pin,
        ServeAuthType::Password => ServeAuthArg::Password,
    });

    match auth {
        ServeAuthArg::None => Ok(ServeAuthRuntime::None),
        ServeAuthArg::Pin => {
            if !matches!(mode, ServeModeArg::LanAuth) {
                bail!("--auth pin can only be used with --mode lan-auth");
            }
            let mut rng = rand::thread_rng();
            let pin = format!("{:06}", rng.gen_range(0..1_000_000));
            Ok(ServeAuthRuntime::Pin(pin))
        }
        ServeAuthArg::Password => {
            if !matches!(mode, ServeModeArg::LanAuth) {
                bail!("--auth password can only be used with --mode lan-auth");
            }
            let hash = cfg
                .serve
                .auth
                .password_hash
                .clone()
                .ok_or_else(|| anyhow::anyhow!(
                    "Password auth requires serve.auth.password_hash in config. Use `bootty config set-password --prompt`."
                ))?;
            Ok(ServeAuthRuntime::PasswordHash(hash))
        }
    }
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => bail!("Invalid bool value: {}", value),
    }
}

fn parse_string_array(value: &str) -> Result<Vec<String>> {
    let trimmed = value.trim();
    if trimmed.starts_with('[') {
        let arr: Vec<String> = serde_json::from_str(trimmed)
            .with_context(|| format!("Invalid JSON string array: {}", value))?;
        if arr.is_empty() {
            bail!("Array must not be empty");
        }
        return Ok(arr);
    }

    let list = trimmed
        .split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    if list.is_empty() {
        bail!("Array must not be empty");
    }
    Ok(list)
}

fn parse_mode(value: &str) -> Result<ServeMode> {
    match value.trim() {
        "local" => Ok(ServeMode::Local),
        "lan-open" => Ok(ServeMode::LanOpen),
        "lan-auth" => Ok(ServeMode::LanAuth),
        _ => bail!("Invalid serve mode: {}", value),
    }
}

fn parse_auth_type(value: &str) -> Result<ServeAuthType> {
    match value.trim() {
        "none" => Ok(ServeAuthType::None),
        "pin" => Ok(ServeAuthType::Pin),
        "password" => Ok(ServeAuthType::Password),
        _ => bail!("Invalid auth type: {}", value),
    }
}

fn get_config_value(cfg: &BoottyConfig, key: &str) -> Result<String> {
    match key {
        "version" => Ok(cfg.version.to_string()),
        "network.stun_servers" => Ok(serde_json::to_string(&cfg.network.stun_servers)?),
        "host.default_cmd" => Ok(serde_json::to_string(&cfg.host.default_cmd)?),
        "host.non_interactive" => Ok(cfg.host.non_interactive.to_string()),
        "serve.host" => Ok(cfg.serve.host.clone()),
        "serve.port" => Ok(cfg.serve.port.to_string()),
        "serve.max_sessions" => Ok(cfg.serve.max_sessions.to_string()),
        "serve.mode" => Ok(match cfg.serve.mode {
            ServeMode::Local => "local",
            ServeMode::LanOpen => "lan-open",
            ServeMode::LanAuth => "lan-auth",
        }
        .to_string()),
        "serve.auth.type" => Ok(match cfg.serve.auth.auth_type {
            ServeAuthType::None => "none",
            ServeAuthType::Pin => "pin",
            ServeAuthType::Password => "password",
        }
        .to_string()),
        "serve.auth.password_hash" => Ok(cfg
            .serve
            .auth
            .password_hash
            .clone()
            .unwrap_or_else(|| "null".to_string())),
        _ => bail!("Unsupported config key: {}", key),
    }
}

fn set_config_value(cfg: &mut BoottyConfig, key: &str, value: &str) -> Result<()> {
    match key {
        "network.stun_servers" => {
            cfg.network.stun_servers = parse_string_array(value)?;
        }
        "host.default_cmd" => {
            cfg.host.default_cmd = parse_string_array(value)?;
        }
        "host.non_interactive" => {
            cfg.host.non_interactive = parse_bool(value)?;
        }
        "serve.host" => {
            let host = value.trim();
            if host.is_empty() {
                bail!("serve.host must not be empty");
            }
            cfg.serve.host = host.to_string();
        }
        "serve.port" => {
            cfg.serve.port = value
                .trim()
                .parse::<u16>()
                .with_context(|| format!("Invalid port value: {}", value))?;
        }
        "serve.max_sessions" => {
            let parsed = value
                .trim()
                .parse::<usize>()
                .with_context(|| format!("Invalid max_sessions value: {}", value))?;
            if parsed == 0 {
                bail!("serve.max_sessions must be greater than 0");
            }
            cfg.serve.max_sessions = parsed;
        }
        "serve.mode" => {
            cfg.serve.mode = parse_mode(value)?;
        }
        "serve.auth.type" => {
            let auth_type = parse_auth_type(value)?;
            if auth_type == ServeAuthType::Password && cfg.serve.auth.password_hash.is_none() {
                bail!(
                    "Cannot set serve.auth.type=password without password hash. Use `bootty config set-password --prompt` first."
                );
            }
            cfg.serve.auth.auth_type = auth_type;
        }
        "serve.auth.password_hash" => {
            let hash = value.trim();
            if hash.eq_ignore_ascii_case("null") || hash.is_empty() {
                cfg.serve.auth.password_hash = None;
            } else {
                cfg.serve.auth.password_hash = Some(hash.to_string());
            }
        }
        _ => bail!("Unsupported config key: {}", key),
    }

    Ok(())
}

fn unset_config_value(cfg: &mut BoottyConfig, key: &str) -> Result<()> {
    match key {
        "network.stun_servers" => {
            cfg.network.stun_servers = vec!["stun:stun.l.google.com:19302".to_string()];
        }
        "host.default_cmd" => {
            cfg.host.default_cmd = vec!["bash".to_string(), "-l".to_string()];
        }
        "host.non_interactive" => {
            cfg.host.non_interactive = false;
        }
        "serve.host" => {
            cfg.serve.host = "127.0.0.1".to_string();
        }
        "serve.port" => {
            cfg.serve.port = 2234;
        }
        "serve.max_sessions" => {
            cfg.serve.max_sessions = 128;
        }
        "serve.mode" => {
            cfg.serve.mode = ServeMode::Local;
        }
        "serve.auth.type" => {
            cfg.serve.auth.auth_type = ServeAuthType::None;
        }
        "serve.auth.password_hash" => {
            cfg.serve.auth.password_hash = None;
        }
        _ => bail!("Unsupported config key: {}", key),
    }

    Ok(())
}

fn print_session_table(sessions: &[AdminSessionItem]) {
    println!(
        "{:<14} {:<10} {:<18} {:<12} {:<12} {}",
        "SESSION_ID", "STATE", "CLIENT_ADDR", "CREATED", "LAST_ACTIVE", "CMD"
    );

    for item in sessions {
        println!(
            "{:<14} {:<10} {:<18} {:<12} {:<12} {}",
            item.session_id,
            format!("{:?}", item.state).to_lowercase(),
            item.client_addr.clone().unwrap_or_else(|| "-".to_string()),
            item.created_at_unix,
            item.last_active_at_unix,
            item.cmd_preview
        );
    }
}

fn validate_config_edit(cfg: &BoottyConfig) -> Result<()> {
    if cfg.serve.mode == ServeMode::LanAuth && cfg.serve.auth.auth_type == ServeAuthType::None {
        bail!("Invalid config transition: serve.mode=lan-auth requires serve.auth.type != none");
    }
    if cfg.serve.auth.auth_type == ServeAuthType::Password && cfg.serve.auth.password_hash.is_none() {
        bail!("Invalid config transition: serve.auth.type=password requires serve.auth.password_hash");
    }
    Ok(())
}
