use std::{net::SocketAddr, sync::Arc, time::Duration};

use rusthouse::{
    QueryError, QueryFuture, QueryRequest, QueryService, ServiceHealth,
    http::{ServerConfig, spawn_http_server},
};

const HELP: &str = "\
RustHouse analytical database

Usage:
  rusthouse serve [OPTIONS]

Options:
  --bind ADDRESS                 Listen address [default: 127.0.0.1:8080]
  --max-request-bytes BYTES      Maximum SQL request size [default: 1048576]
  --max-response-bytes BYTES     Maximum encoded result size [default: 16777216]
  --max-concurrent-queries N     Query execution slots [default: 16]
  --max-concurrent-requests N    HTTP query request slots [default: 64]
  --request-body-timeout-ms MS   Request body deadline [default: 10000]
  --query-timeout-ms MS          Per-query deadline [default: 30000]
  --shutdown-timeout-ms MS       Graceful shutdown window [default: 10000]
  -h, --help                     Print help
";

struct EngineNotInstalled;

impl QueryService for EngineNotInstalled {
    fn execute(&self, _request: QueryRequest) -> QueryFuture<'_> {
        Box::pin(async {
            Err(QueryError::unavailable(
                "no SQL engine is configured for this server",
            ))
        })
    }

    fn health(&self) -> ServiceHealth {
        ServiceHealth::not_ready("no SQL engine is configured")
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("rusthouse: {error}");
        std::process::exit(2);
    }
}

async fn run() -> Result<(), String> {
    let mut arguments = std::env::args().skip(1);
    let Some(command) = arguments.next() else {
        println!(
            "{}: the analytical engine is warming up",
            rusthouse::product_name()
        );
        println!("Run `rusthouse --help` for usage.");
        return Ok(());
    };
    if matches!(command.as_str(), "-h" | "--help") {
        print!("{HELP}");
        return Ok(());
    }
    if command != "serve" {
        return Err(format!("unknown command `{command}`\n\n{HELP}"));
    }

    let mut address: SocketAddr = "127.0.0.1:8080"
        .parse()
        .expect("the default address is valid");
    let mut config = ServerConfig::default();
    while let Some(option) = arguments.next() {
        if matches!(option.as_str(), "-h" | "--help") {
            print!("{HELP}");
            return Ok(());
        }
        let value = arguments
            .next()
            .ok_or_else(|| format!("missing value for `{option}`"))?;
        match option.as_str() {
            "--bind" => {
                address = value
                    .parse()
                    .map_err(|error| format!("invalid --bind address: {error}"))?;
            }
            "--max-request-bytes" => {
                config.max_request_bytes = parse_number(&option, &value)?;
            }
            "--max-response-bytes" => {
                config.max_response_bytes = parse_number(&option, &value)?;
            }
            "--max-concurrent-queries" => {
                config.max_concurrent_queries = parse_number(&option, &value)?;
            }
            "--max-concurrent-requests" => {
                config.max_concurrent_requests = parse_number(&option, &value)?;
            }
            "--request-body-timeout-ms" => {
                config.request_body_timeout = Duration::from_millis(parse_number(&option, &value)?);
            }
            "--query-timeout-ms" => {
                config.query_timeout = Duration::from_millis(parse_number(&option, &value)?);
            }
            "--shutdown-timeout-ms" => {
                config.shutdown_timeout = Duration::from_millis(parse_number(&option, &value)?);
            }
            _ => return Err(format!("unknown option `{option}`")),
        }
    }

    let mut shutdown_signals = ShutdownSignals::new()?;
    let server = spawn_http_server(address, Arc::new(EngineNotInstalled), config)
        .await
        .map_err(|error| error.to_string())?;
    eprintln!(
        "RustHouse HTTP server listening on http://{}",
        server.local_addr()
    );
    shutdown_signals.wait().await?;
    server.shutdown().await.map_err(|error| error.to_string())
}

struct ShutdownSignals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl ShutdownSignals {
    fn new() -> Result<Self, String> {
        #[cfg(unix)]
        {
            let interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                    .map_err(|error| format!("could not listen for SIGINT: {error}"))?;
            let terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .map_err(|error| format!("could not listen for SIGTERM: {error}"))?;
            Ok(Self {
                interrupt,
                terminate,
            })
        }

        #[cfg(not(unix))]
        Ok(Self {})
    }

    async fn wait(&mut self) -> Result<(), String> {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.interrupt.recv() => Ok(()),
                _ = self.terminate.recv() => Ok(()),
            }
        }

        #[cfg(not(unix))]
        tokio::signal::ctrl_c()
            .await
            .map_err(|error| format!("could not listen for Ctrl-C: {error}"))
    }
}

fn parse_number<T>(option: &str, value: &str) -> Result<T, String>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|error| format!("invalid value for `{option}`: {error}"))
}
