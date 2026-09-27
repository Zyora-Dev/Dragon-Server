use clap::{Parser, Subcommand};
use dragon_server::{config::Config, server::Server};
use std::{path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(name = "dragon", version, about = "Dragon HTTP server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Start {
        #[arg(long)]
        config: PathBuf,
    },
}

fn main() -> ExitCode {
    let Command::Start { config } = Cli::parse().command;
    let config = match Config::load(&config) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("dragon: configuration error: {error}");
            return ExitCode::from(2);
        }
    };
    let level: tracing::Level = config.logging.level.parse().expect("validated log level");
    let (writer, _log_guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .buffered_lines_limit(1024)
        .lossy(true)
        .finish(std::io::stderr());
    let dropped = writer.error_counter();
    tracing_subscriber::fmt()
        .json()
        .with_max_level(level)
        .with_writer(writer)
        .init();
    let server = match Server::new(config) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("dragon: cannot prepare server: {error}");
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(16)
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("dragon: runtime error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let outcome = runtime.block_on(async {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            let mut previous = 0;
            loop {
                interval.tick().await;
                let count = dropped.dropped_lines();
                if count != previous {
                    tracing::warn!(event = "log_overflow", dropped_lines = count);
                    previous = count;
                }
            }
        });
        #[cfg(unix)]
        {
            let mut interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            server
                .run(async move {
                    tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
                })
                .await
        }
        #[cfg(not(unix))]
        server
            .run(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await
    });
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("dragon: server error: {error}");
            ExitCode::FAILURE
        }
    }
}
