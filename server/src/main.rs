use std::sync::Arc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use brickbed_server::build_router;
use brickbed_server::config::{instance_name, Config};
use brickbed_server::db::Db;
use brickbed_server::handlers::AppState;
use brickbed_server::jwt::{HttpJwksFetcher, JwksCache};
use brickbed_server::keybroker::{
    generate_secret_hex, parse_keygen_args, KeyBroker, KeygenCommand, KEYGEN_USAGE,
};

/// `keygen` mints instance keys from `INSTANCE_SECRET`; anything else serves.
fn keygen(args: &[String]) -> Result<String, String> {
    let command = parse_keygen_args(args).map_err(|e| format!("{}\n\n{}", e, KEYGEN_USAGE))?;

    let project = match command {
        KeygenCommand::GenerateSecret => return Ok(generate_secret_hex()),
        KeygenCommand::IssueKey { project } => project,
    };

    let secret = std::env::var("INSTANCE_SECRET").map_err(|_| {
        "INSTANCE_SECRET is not set. Generate one with `brickbed-server keygen \
         --generate-secret`, then set it on both this CLI and the server."
            .to_string()
    })?;

    let broker = KeyBroker::from_hex(&instance_name(), &secret).map_err(|e| e.to_string())?;
    broker.issue(&project).map_err(|e| e.to_string())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("keygen") {
        match keygen(&args[1..]) {
            Ok(output) => println!("{}", output),
            Err(message) => {
                eprintln!("error: {}", message);
                std::process::exit(1);
            }
        }
        return;
    }

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "brickbed_server=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Config::from_env();
    tracing::info!(
        storage_backend = config.storage.kind(),
        "storage backend configured"
    );

    let db = Db::open(&config).await.expect("Failed to open database");
    let state = Arc::new(AppState {
        db,
        api_keys: config.api_keys.clone(),
        keybroker: config.keybroker.clone(),
        jwks: JwksCache::new(HttpJwksFetcher::new()),
    });

    let app = build_router(state.clone());

    let addr = config.addr();
    tracing::info!("Starting server on {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();

    match Arc::try_unwrap(state) {
        Ok(state) => {
            if state.db.close().await.is_err() {
                tracing::error!(
                    failure = "database_close_failed",
                    "database close failed during shutdown"
                );
            }
        }
        Err(_) => tracing::error!("database still has live references after server shutdown"),
    }
}
