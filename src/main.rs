mod client;
mod config;
mod host;
mod pty;
mod sdp;
mod serve;
mod session;
mod ten_kb_site;
mod terminal;

use clap::Parser;
use client::ClientSession;
use config::load_or_create_config;
use host::HostSession;
use std::env;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "bootty")]
#[command(about = "Share a terminal session over WebRTC")]
struct Cli {
    #[arg(short = 'o', help = "One-way connection with no response needed")]
    one_way: bool,

    #[arg(short = 'v', help = "Verbose logging")]
    verbose: bool,

    #[arg(long = "non-interactive", help = "Set host to non-interactive")]
    non_interactive: bool,

    #[arg(short = 'n', long = "ni", help = "Set host to non-interactive")]
    ni: bool,

    #[arg(
        short = 's',
        default_value = "stun:stun.l.google.com:19302",
        help = "The stun server to use"
    )]
    stun_server: String,

    #[arg(
        long = "serve",
        value_name = "PORT",
        num_args = 0..=1,
        default_missing_value = "2234",
        help = "Start a web server for browser-based terminal access. Optional port (default: 2234)"
    )]
    serve: Option<u16>,

    #[arg(help = "Connection string (for client mode)")]
    offer: Option<String>,
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let (cmd, filtered_args) = extract_cmd(args);

    let cli = Cli::parse_from(filtered_args);

    if cli.verbose {
        let filter = env::var("RUST_LOG").unwrap_or_else(|_| {
            "info,bootty=trace,webrtc=info,webrtc_ice=warn,webrtc_sctp=warn,webrtc_mdns=warn,webrtc_dtls=warn".to_string()
        });
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .with_env_filter(EnvFilter::new(filter))
            .try_init();
    }

    let stun_servers = vec![cli.stun_server];
    // In serve mode, default to non-interactive to avoid interleaving PTY output with logs.
    let non_interactive = cli.non_interactive || cli.ni || cli.serve.is_some();
    let config = match load_or_create_config() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Failed to load config: {e:?}");
            std::process::exit(1);
        }
    };

    let result = if let Some(offer) = cli.offer {
        let mut client = match ClientSession::new(offer, stun_servers).await {
            Ok(session) => session,
            Err(e) => {
                eprintln!("Failed to create client session: {e:?}");
                std::process::exit(1);
            }
        };
        client.run().await
    } else {
        let mut host = match HostSession::new(
            cmd,
            non_interactive,
            cli.one_way,
            stun_servers,
            cli.serve,
            config.max_sessions,
        )
        .await
        {
            Ok(session) => session,
            Err(e) => {
                eprintln!("Failed to create host session: {e:?}");
                std::process::exit(1);
            }
        };
        host.run().await
    };

    if let Err(e) = result {
        eprintln!("Fatal error: \"{e}\"");
        std::process::exit(1);
    }
}

fn extract_cmd(args: Vec<String>) -> (Vec<String>, Vec<String>) {
    let mut cmd = vec!["bash".to_string(), "-l".to_string()];
    let mut filtered = Vec::new();
    let mut skip_next = false;
    let mut found_cmd = false;

    for (i, arg) in args.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "-cmd" {
            cmd = args[i + 1..].to_vec();
            found_cmd = true;
            break;
        }
        filtered.push(arg.clone());
    }

    if !found_cmd {
        // Keep default cmd and restore full args
        filtered = args;
    }

    (cmd, filtered)
}
