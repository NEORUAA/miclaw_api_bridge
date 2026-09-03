use clap::{Parser, Subcommand};
use miclaw_api_bridge_lib::auth::login::{LoginOutcome, LoginRequest};
use miclaw_api_bridge_lib::error::{BridgeError, Result};
use miclaw_api_bridge_lib::mimo::{known_models, AuthSnapshot, ModelInfo, QuotaSnapshot};
use miclaw_api_bridge_lib::proxy::ProxySnapshot;
use miclaw_api_bridge_lib::server::{start_http, ServerConfig};
use miclaw_api_bridge_lib::service::{SendTicketRequest, SetPortRequest, VerifyTicketRequest};
use miclaw_api_bridge_lib::state::BridgeState;
use miclaw_api_bridge_lib::storage::Storage;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::net::{IpAddr, Ipv4Addr};

#[derive(Parser)]
#[command(
    name = "miclaw_api_bridge",
    version,
    about = "Super XiaoAI MiMo local API bridge"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the headless WebUI + OpenAI/Anthropic proxy server.
    Server {
        #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::LOCALHOST))]
        host: IpAddr,
        #[arg(long)]
        port: Option<u16>,
        /// Serve over HTTPS (self-signed cert auto-generated if no cert given).
        #[arg(long)]
        tls: bool,
        /// Custom TLS certificate chain (PEM). Implies --tls.
        #[arg(long)]
        tls_cert: Option<String>,
        /// Custom TLS private key (PEM). Implies --tls.
        #[arg(long)]
        tls_key: Option<String>,
    },
    /// Show auth and proxy status from a running local server.
    Status {
        #[arg(long)]
        base_url: Option<String>,
    },
    /// Log in through a running local server.
    Login {
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        account: String,
        #[arg(long)]
        password: String,
        #[arg(long)]
        captcha: Option<String>,
    },
    /// Send Xiaomi two-factor ticket through a running local server.
    SendTicket {
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        flag: i32,
    },
    /// Verify Xiaomi two-factor ticket through a running local server.
    VerifyTicket {
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        flag: i32,
        #[arg(long)]
        ticket: String,
    },
    /// Refresh the persisted Xiaomi service token through a running local server.
    Refresh {
        #[arg(long)]
        base_url: Option<String>,
    },
    /// Log out through a running local server.
    Logout {
        #[arg(long)]
        base_url: Option<String>,
    },
    /// List known models.
    Models {
        #[arg(long)]
        base_url: Option<String>,
    },
    /// Save the WebUI/proxy port. A running server must be restarted to use it.
    SetPort {
        port: u16,
        #[arg(long)]
        base_url: Option<String>,
    },
    /// Open WebUI in the default browser.
    OpenWebui {
        #[arg(long)]
        base_url: Option<String>,
    },
    /// Print version.
    Version,
}

#[tokio::main]
async fn main() {
    miclaw_api_bridge_lib::init_tracing();
    if let Err(e) = run().await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Server {
        host: IpAddr::V4(Ipv4Addr::LOCALHOST),
        port: None,
        tls: false,
        tls_cert: None,
        tls_key: None,
    }) {
        Command::Server {
            host,
            port,
            tls,
            tls_cert,
            tls_key,
        } => run_server(host, port, tls, tls_cert, tls_key).await,
        Command::Status { base_url } => status(base_url).await,
        Command::Login {
            base_url,
            account,
            password,
            captcha,
        } => {
            let outcome: LoginOutcome = post_json(
                &base(base_url)?,
                "/api/auth/login",
                &LoginRequest {
                    account,
                    password,
                    captcha,
                },
            )
            .await?;
            print_json(&outcome)
        }
        Command::SendTicket { base_url, flag } => {
            let sent: bool = post_json(
                &base(base_url)?,
                "/api/auth/two-factor/send",
                &SendTicketRequest { flag },
            )
            .await?;
            println!(
                "{}",
                if sent {
                    "ticket sent"
                } else {
                    "ticket not sent"
                }
            );
            Ok(())
        }
        Command::VerifyTicket {
            base_url,
            flag,
            ticket,
        } => {
            let _: Value = post_json(
                &base(base_url)?,
                "/api/auth/two-factor/verify",
                &VerifyTicketRequest { flag, ticket },
            )
            .await?;
            println!("verified");
            Ok(())
        }
        Command::Refresh { base_url } => {
            let auth: AuthSnapshot = post_json(&base(base_url)?, "/api/auth/refresh", &()).await?;
            print_json(&auth)
        }
        Command::Logout { base_url } => {
            let _: Value = post_json(&base(base_url)?, "/api/auth/logout", &()).await?;
            println!("logged out");
            Ok(())
        }
        Command::Models { base_url } => models(base_url).await,
        Command::SetPort { port, base_url } => set_port(port, base_url).await,
        Command::OpenWebui { base_url } => {
            let url = base(base_url)?;
            open::that(&url).map_err(BridgeError::other)?;
            println!("opened {url}");
            Ok(())
        }
        Command::Version => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

async fn run_server(
    host: IpAddr,
    port: Option<u16>,
    tls: bool,
    tls_cert: Option<String>,
    tls_key: Option<String>,
) -> Result<()> {
    let state = BridgeState::new()?;
    // TLS is flag-authoritative: each `server` run sets the persisted TLS state
    // from the CLI flags, so `--tls` enables it and a plain run disables it.
    // On OpenWrt the init script passes these flags based on the `tls` UCI option.
    state.storage.update_settings(|s| {
        s.tls_enabled = tls || tls_cert.is_some() || tls_key.is_some();
        s.tls_cert_path = tls_cert.clone();
        s.tls_key_path = tls_key.clone();
    })?;
    let port = port.unwrap_or_else(|| state.storage.settings().proxy_port);
    let server = start_http(state, ServerConfig { host, port }).await?;
    println!(
        "miclaw_api_bridge server listening on {}",
        server.webui_url()
    );
    println!("webui: {}", server.webui_url());
    println!("openai base: {}/v1", server.webui_url());
    tokio::signal::ctrl_c().await.map_err(BridgeError::from)?;
    server.shutdown();
    Ok(())
}

async fn status(base_url: Option<String>) -> Result<()> {
    let base = base(base_url)?;
    match get_json::<ProxySnapshot>(&base, "/api/proxy/status").await {
        Ok(proxy) => {
            let auth: AuthSnapshot = get_json(&base, "/api/auth/status").await?;
            println!("server: running");
            println!("webui: {base}");
            println!("proxy: {:?}", proxy);
            println!(
                "auth: {}",
                if auth.authenticated {
                    auth.nick
                        .or(auth.user_id)
                        .unwrap_or_else(|| "authenticated".into())
                } else {
                    "not authenticated".into()
                }
            );
            if auth.authenticated {
                match get_json::<QuotaSnapshot>(&base, "/api/quota").await {
                    Ok(quota) => println!(
                        "quota: {}/{} points remaining (used {}, status {})",
                        quota.points_remaining, quota.points_limit, quota.points_used, quota.status
                    ),
                    Err(_) => println!("quota: unavailable"),
                }
            }
            Ok(())
        }
        Err(_) => {
            let storage = Storage::new()?;
            println!("server: offline");
            println!("configured port: {}", storage.settings().proxy_port);
            Ok(())
        }
    }
}

async fn models(base_url: Option<String>) -> Result<()> {
    let base = base(base_url)?;
    let models = match get_json::<Vec<ModelInfo>>(&base, "/api/models").await {
        Ok(models) => models,
        Err(_) => known_models(),
    };
    for model in models {
        println!("{}\t{}", model.id, model.family);
    }
    Ok(())
}

async fn set_port(port: u16, base_url: Option<String>) -> Result<()> {
    if let Some(base_url) = base_url {
        let snapshot: ProxySnapshot =
            post_json(&base_url, "/api/settings/port", &SetPortRequest { port }).await?;
        print_json(&snapshot)?;
        println!("restart the server for the new port to take effect");
        return Ok(());
    }

    let base = base(None)?;
    if let Ok(snapshot) =
        post_json::<ProxySnapshot, _>(&base, "/api/settings/port", &SetPortRequest { port }).await
    {
        print_json(&snapshot)?;
        println!("restart the server for the new port to take effect");
        return Ok(());
    }

    let storage = Storage::new()?;
    storage.update_settings(|s| s.proxy_port = port)?;
    println!("saved port {port}; restart the server for it to take effect");
    Ok(())
}

fn base(base_url: Option<String>) -> Result<String> {
    if let Some(url) = base_url {
        return Ok(url.trim_end_matches('/').to_string());
    }
    let storage = Storage::new()?;
    Ok(format!(
        "http://127.0.0.1:{}",
        storage.settings().proxy_port
    ))
}

async fn get_json<T: DeserializeOwned>(base: &str, path: &str) -> Result<T> {
    let url = format!("{base}{path}");
    let resp = reqwest::get(url).await?;
    decode_json(resp).await
}

async fn post_json<T: DeserializeOwned, B: Serialize>(
    base: &str,
    path: &str,
    body: &B,
) -> Result<T> {
    let url = format!("{base}{path}");
    let client = reqwest::Client::new();
    let resp = client.post(url).json(body).send().await?;
    decode_json(resp).await
}

async fn decode_json<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            if let Some(msg) = v
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
            {
                return Err(BridgeError::Other(msg.to_string()));
            }
        }
        return Err(BridgeError::Other(format!("http {status}: {text}")));
    }
    Ok(serde_json::from_str(&text)?)
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
