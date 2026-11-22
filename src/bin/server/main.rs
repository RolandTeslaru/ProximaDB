//! ProximaDB's HTTP server: JSON over HTTP/1.1 with keep-alive, on a Unix domain socket and/or a TCP port.
//!
//! - Connections and HTTP: tokio + axum, a few threads for any number of connections.
//! - Reads: each request takes the current generation and runs on the rayon pool (CPU and memory bound work never
//!   runs on tokio's threads). At most one read per core runs at a time; the rest wait their turn.
//! - Writes: the one writer thread, fed by a bounded channel. Each action carries Quarry's sequence number and is
//!   published whole. When no action arrives for a while the writer runs its idle check (rebuilds).
//! - Read-your-writes: a read with `after: N` waits until action N is published.
//!
//! Every request is checked before it reaches the engine: a release build aborts on a panic, so a wrong name or value
//! must be a 400, never a panic.
//!
//!     server --schema schema.json --data <dir> [--unix /tmp/proximadb.sock] [--tcp 127.0.0.1:7600]

mod api;
mod check;
mod reads;
mod schema;
mod state;
mod writes;

use std::future::IntoFuture;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use axum::serve::ListenerExt;
use tokio::sync::{Semaphore, mpsc, watch};

use proximadb::Store;
use proximadb::load;

use crate::state::AppState;

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut schema, mut data, mut unix, mut tcp) = (None, None, None, None);
    while let Some(a) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| panic!("{a} needs a value"));
        match a.as_str() {
            "--schema" => schema = Some(PathBuf::from(value())),
            "--data" => data = Some(PathBuf::from(value())),
            "--unix" => unix = Some(PathBuf::from(value())),
            "--tcp" => tcp = Some(value()),
            other => panic!("unknown argument {other}"),
        }
    }
    let schema = schema::schema_from(&schema.expect("--schema")).unwrap_or_else(|e| panic!("schema: {e}"));
    if unix.is_none() && tcp.is_none() {
        tcp = Some("127.0.0.1:7600".into());
    }
    let started = std::time::Instant::now();
    let graph = load::load(&data.expect("--data"), &schema).unwrap_or_else(|e| panic!("load: {e}"));
    eprintln!("loaded {} nodes in {:.2} s", graph.node_count(), started.elapsed().as_secs_f64());

    let store: &'static Store = Box::leak(Box::new(Store::new(graph)));
    let (writes, rx) = mpsc::channel(writes::WRITE_QUEUE);
    let (published_tx, published) = watch::channel(store.current().live.published());
    std::thread::Builder::new()
        .name("writer".into())
        .spawn(move || writes::writer_thread(store, rx, published_tx))
        .unwrap();
    let state = Arc::new(AppState {
        store,
        reads: Semaphore::new(rayon::current_num_threads()),
        writes,
        published,
    });
    let app = Router::new()
        .route("/health", get(reads::health))
        .route("/reach", post(reads::reach))
        .route("/path", post(reads::path))
        .route("/actions", post(writes::action))
        .with_state(state);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let mut servers = Vec::new();
        if let Some(path) = unix {
            let _ = std::fs::remove_file(&path);
            let listener = tokio::net::UnixListener::bind(&path).expect("bind unix socket");
            eprintln!("listening on unix:{}", path.display());
            servers.push(tokio::spawn(axum::serve(listener, app.clone()).into_future()));
        }
        if let Some(addr) = tcp {
            let listener = tokio::net::TcpListener::bind(&addr)
                .await
                .expect("bind tcp")
                .tap_io(|tcp| {
                    let _ = tcp.set_nodelay(true);
                });
            eprintln!("listening on http://{addr}");
            servers.push(tokio::spawn(axum::serve(listener, app).into_future()));
        }
        for s in servers {
            let _ = s.await;
        }
    });
}
