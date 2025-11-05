use super::files::import;
use super::state::{SendHandle, SendStatus};
use anyhow::bail;
use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};

use iroh_blobs::{api::Store, Hash};
use ngrok::config::ForwarderBuilder;
use ngrok;
use ngrok::tunnel::EndpointInfo;
use std::{path::PathBuf, sync::Arc};
use tokio::{runtime::Handle as TokioHandle, sync::mpsc};
use tokio_util::io::ReaderStream;
use url::Url;

/// Public entry point for starting an HTTP (web link) send operation.
pub(crate) async fn start_http_send_internal(
    path: PathBuf,
    progress_sender: mpsc::Sender<SendStatus>,
    tokio_handle: TokioHandle,
) -> anyhow::Result<SendHandle> {
    progress_sender.send(SendStatus::Connecting).await?;

    let suffix: [u8; 8] = rand::random();
    let data_dir = std::env::temp_dir().join(format!("p2p-client-http-{}", hex::encode(suffix)));
    tokio::fs::create_dir_all(&data_dir).await?;
    let db = iroh_blobs::store::fs::FsStore::load(&data_dir).await?;
    let (_temp_tag, _size, collection) = import(&path, &db, progress_sender.clone()).await?;

    let (download_hash, file_name) = if collection.len() == 1 {
        let (name, hash) = collection.iter().next().ok_or_else(|| anyhow::anyhow!("Collection is empty"))?;
        (*hash, name.clone())
    } else {
        bail!("Sending directories via web link is not yet supported. Please select a single file.");
    };

    let app_state = AppState {
        db: Arc::new(db.into()),
        file_name,
    };

    let app = Router::new()
        .route("/download/{hash}", get(download_handler))
        .with_state(app_state);

    progress_sender.send(SendStatus::Connecting).await?;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let local_addr = listener.local_addr()?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .with_graceful_shutdown(async {
                shutdown_rx.await.ok();
            })
            .await
            .unwrap();
        println!("Shutting down HTTP server...");
    });

    let authtoken = std::env::var("NGROK_AUTHTOKEN").ok();

    let mut session_builder = ngrok::Session::builder();
    if let Some(token) = authtoken {
        println!("Using token from NGROK_AUTHTOKEN env var.");
        session_builder.authtoken(token);
    } else {
        println!("Trying to find token in default config file...");
        session_builder.authtoken_from_env();
    };

    let to_url_str = format!("http://{}", local_addr);
    let to_url = to_url_str.parse::<Url>()?;

    let tun = session_builder
        .connect()
        .await?
        .http_endpoint()
        .listen_and_forward(to_url) 
        .await?;

    let url = format!("{}/download/{}", tun.url(), download_hash);
    println!("ngrok tunnel started at: {}", url);

    progress_sender
        .send(SendStatus::ReadyToSend { ticket: url })
        .await?;

    Ok(SendHandle {
        data_dir,
        shutdown_tx: Some(shutdown_tx),
        _ngrok_tunnel: Some(tun),
        tokio_handle,
    })
}

/// State for the Axum web server.
#[derive(Clone)]
struct AppState {
    db: Arc<Store>,
    file_name: String,
}
/// Axum handler to process a download request.
async fn download_handler(
    State(state): State<AppState>,
    AxumPath(hash_str): AxumPath<String>,
) -> impl IntoResponse {
    let hash = match hash_str.parse::<Hash>() {
        Ok(h) => h,
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid hash format").into_response(),
    };

    if !state.db.has(hash).await.unwrap_or(false) {
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    }

    let reader = state.db.reader(hash);
    let stream = ReaderStream::new(reader);
    let body = Body::from_stream(stream);

    let disposition = format!("attachment; filename=\"{}\"", state.file_name);

    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_DISPOSITION, disposition)
        .body(body)
        .unwrap()
        .into_response()
}