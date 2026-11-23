//! ProximaDB's HTTP server: JSON over HTTP/1.1 with keep-alive, on a Unix domain socket and/or a TCP port.
//!
//! Startup, in order:
//!
//!   1. Read the command line (`config`) and the schema file (`schema`).
//!   2. Load the graph from the data files into a `Store`.
//!   3. Start the writer thread (`writes`): the one thread that applies actions.
//!   4. Serve the routes on each listener, until SIGTERM or Ctrl-C: then finish the requests running and exit.
//!
//! While serving:
//!
//! - Connections and HTTP: tokio + axum, a few threads for any number of connections.
//! - Reads (`reads`): each request takes the current generation and runs on the rayon pool, never on tokio's
//!   threads. At most one read per core runs at a time; the rest wait their turn.
//! - Writes (`writes`): actions go through a bounded queue to the writer thread. Each carries Quarry's sequence
//!   number and is published whole. With no action for a while, the writer runs its idle check (rebuilds).
//! - Cypher (`cypher`): a subset of Cypher, answered as a graph to draw and a count.
//! - Read-your-writes: a read with `after: N` waits until action N is published.
//! - Reloads (`reload`): Quarry's sync rewrote the datasets; load them again and swap the new graph in.
//!
//! Every request is checked (`check`) before it reaches the engine: a release build aborts on a panic, so a wrong
//! name or value must be a 400, never a panic.
//!
//!     server --schema schema.json --data <dir> [--unix /tmp/proximadb.sock] [--tcp 127.0.0.1:7600]

mod api;
mod check;
mod config;
mod cypher;
mod reads;
mod reload;
mod schema;
mod state;
mod writes;

use std::future::IntoFuture;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use axum::Router;
use axum::routing::{get, post};
use axum::serve::ListenerExt;
use tokio::net::{TcpListener, UnixListener};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinHandle;

use proximadb::Store;
use proximadb::load;

use crate::config::Config;
use crate::state::{AppState, Shared, Source};

/// musl's own allocator is slow with many threads; mimalloc is fast everywhere.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Threads for connections and HTTP; the queries themselves run on rayon's pool.
const HTTP_THREADS: usize = 2;

/// Any user that can see the socket may connect: it lives in a directory shared only with the client (a volume
/// between two containers, or a directory with its own permissions).
const SOCKET_MODE: u32 = 0o666;


fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,

        Err(message) => {
            eprintln!("proximadb: {message}");
            ExitCode::FAILURE
        }
    }
}


fn run() -> Result<(), String> {
    let config = Config::from_args(std::env::args().skip(1))?;

    let store = load_store(&config)?;

    let source = Source {
        schema: config.schema.clone(),
        data: config.data.clone(),
    };

    let state = start_writer(store, source);

    serve(config, router(state))
}


/// Loads the graph once; the store lives as long as the process, so it is leaked to a `'static` reference that
/// the writer thread and every request can hold.
fn load_store(config: &Config) -> Result<&'static Store, String> {
    let schema = schema::schema_from(&config.schema).map_err(|e| format!("schema: {e}"))?;

    let started = Instant::now();
    let (graph, report) =
        load::load_with_report(&config.data, &schema).map_err(|e| format!("load: {e}"))?;

    eprintln!(
        "loaded {} nodes in {:.2} s",
        graph.node_count(),
        started.elapsed().as_secs_f64()
    );

    print_skipped(&report);

    Ok(Box::leak(Box::new(Store::new(graph))))
}


/// What the load skipped: rows without a key or with a duplicate key, and links that are empty or point nowhere.
fn print_skipped(report: &load::Report) {
    for message in reload::skipped_of(report) {
        eprintln!("  {message}");
    }
}


fn start_writer(store: &'static Store, source: Source) -> Shared {
    let (writes, queue) = mpsc::channel(writes::WRITE_QUEUE);
    let (published_sender, published) = watch::channel(store.current().draft.published());

    std::thread::Builder::new()
        .name("writer".into())
        .spawn(move || writes::writer_thread(store, queue, published_sender))
        .expect("the writer thread starts");

    Arc::new(AppState {
        store,
        reads: Semaphore::new(rayon::current_num_threads()),
        writes,
        published,
        source,
        reloading: AtomicBool::new(false),
    })
}


fn router(state: Shared) -> Router {
    Router::new()
        .route("/health", get(reads::health))
        .route("/reach", post(reads::reach))
        .route("/path", post(reads::path))
        .route("/cypher", post(cypher::cypher))
        .route("/actions", post(writes::action))
        .route("/reload", post(reload::reload))
        .with_state(state)
}


/// Serves on every configured listener until SIGTERM (as `docker stop` sends) or Ctrl-C. Then every listener stops
/// taking connections, requests already running finish, and the socket file is removed.
fn serve(config: Config, app: Router) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(HTTP_THREADS)
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;

    runtime.block_on(async move {
        let stopping = stop_signal()?;
        let mut servers = Vec::new();

        if let Some(path) = &config.unix_socket {
            servers.push(serve_unix(path, app.clone(), stopping.clone())?);
        }

        if let Some(address) = &config.tcp_address {
            servers.push(serve_tcp(address, app, stopping).await?);
        }

        for server in servers {
            let _ = server.await;
        }

        if let Some(path) = &config.unix_socket {
            let _ = std::fs::remove_file(path);
        }

        eprintln!("stopped");

        Ok(())
    })
}


/// Changes once SIGTERM or Ctrl-C arrives.
fn stop_signal() -> Result<watch::Receiver<bool>, String> {
    let mut terminate = signal(SignalKind::terminate()).map_err(|e| format!("signals: {e}"))?;
    let (stop, stopping) = watch::channel(false);

    tokio::spawn(async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }

        eprintln!("stopping: finishing the requests already running");
        let _ = stop.send(true);
    });

    Ok(stopping)
}


/// Waits for the stop signal; given to each listener as its graceful shutdown.
async fn stopped(mut stopping: watch::Receiver<bool>) {
    let _ = stopping.wait_for(|&stop| stop).await;
}


fn serve_unix(
    path: &Path,
    app: Router,
    stopping: watch::Receiver<bool>,
) -> Result<JoinHandle<std::io::Result<()>>, String> {
    // A socket file left by an earlier run would make the bind fail.
    let _ = std::fs::remove_file(path);

    let listener = UnixListener::bind(path).map_err(|e| format!("bind {}: {e}", path.display()))?;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_MODE))
        .map_err(|e| format!("socket permissions: {e}"))?;

    eprintln!("listening on unix:{}", path.display());

    let server = axum::serve(listener, app).with_graceful_shutdown(stopped(stopping));

    Ok(tokio::spawn(server.into_future()))
}


async fn serve_tcp(
    address: &str,
    app: Router,
    stopping: watch::Receiver<bool>,
) -> Result<JoinHandle<std::io::Result<()>>, String> {
    let listener = TcpListener::bind(address)
        .await
        .map_err(|e| format!("bind {address}: {e}"))?
        .tap_io(|connection| {
            // Small requests go out at once instead of waiting to be batched.
            let _ = connection.set_nodelay(true);
        });

    eprintln!("listening on http://{address}");

    let server = axum::serve(listener, app).with_graceful_shutdown(stopped(stopping));

    Ok(tokio::spawn(server.into_future()))
}
