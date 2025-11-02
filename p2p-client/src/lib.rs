// Crate-level attribute to suppress a specific Clippy lint.
#![allow(clippy::large_enum_variant)]

use std::{
    path::{Component, Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use anyhow::{bail, Context};
use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use iroh::{Endpoint, RelayMode, SecretKey};
use iroh_blobs::{
    api::{
        blobs::{
            AddPathOptions, AddProgressItem, ExportMode, ExportOptions, ExportProgressItem,
            ImportMode,
        },
        remote::GetProgressItem,
        Store, TempTag,
    },
    format::collection::Collection,
    get::request::get_hash_seq_and_sizes,
    protocol::ALPN as BlobsAlpn,
    ticket::BlobTicket,
    BlobFormat, BlobsProtocol, Hash,
};
use n0_future::StreamExt;
use ngrok::{forwarder::Forwarder, prelude::*};
use rand::Rng;
use tokio::{runtime::Handle as TokioHandle, sync::mpsc};
use tokio_util::io::ReaderStream;
use url::Url;
use walkdir::WalkDir;

/// Defines the states of a send operation for reporting progress to the UI.
#[derive(Debug, Clone)]
pub enum SendStatus {
    Connecting,
    Importing {
        total_files: usize,
        done_files: usize,
        total_size: u64,
        done_size: u64,
    },
    ReadyToSend {
        ticket: String,
    },
    Transferring {
        peer: String,
    },
    PeerDisconnected {
        peer: String,
    },
    Done,
    Error(String),
}

/// Defines the states of a receive operation for reporting progress to the UI.
#[derive(Debug, Clone)]
pub enum ReceiveStatus {
    Connecting,
    Connected { total_files: u64, total_size: u64 },
    Downloading { downloaded: u64, total: u64 },
    Exporting { total_files: u64, done_files: u64 },
    Done,
    Error(String),
}

/// A handle to a running send operation.
/// When this struct is dropped, it automatically cleans up all associated resources.
pub struct SendHandle {
    data_dir: PathBuf,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    _ngrok_tunnel: Option<Forwarder<ngrok::tunnel::HttpTunnel>>,
    tokio_handle: TokioHandle,
}
/// The Drop implementation ensures that background tasks are shut down and temporary files are deleted.
impl Drop for SendHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        // Asynchronously close the ngrok tunnel if it exists.
        if let Some(mut tunnel) = self._ngrok_tunnel.take() {
            self.tokio_handle.spawn(async move {
                let _ = tunnel.close().await;
                println!("Ngrok tunnel closed.");
            });
        }
        // Remove the temporary data directory in a separate thread.
        let data_dir = self.data_dir.clone();
        std::thread::spawn(move || {
            let _ = std::fs::remove_dir_all(data_dir);
        });
        println!("Send operation cancelled and cleaning up.");
    }
}
/// Public entry point for starting a P2P (ticket-based) send operation.
/// This wraps the internal logic to handle errors and send them back to the UI.
pub async fn send_file(
    path: PathBuf,
    progress_sender: mpsc::Sender<SendStatus>,
    tokio_handle: TokioHandle,
) -> anyhow::Result<SendHandle> {
    match send_internal(path, progress_sender.clone(), tokio_handle).await {
        Ok(handle) => Ok(handle),
        Err(e) => {
            progress_sender
                .send(SendStatus::Error(e.to_string()))
                .await
                .ok();
            Err(e)
        }
    }
}
/// Public entry point for starting an HTTP (web link) send operation.
pub async fn start_http_send(
    path: PathBuf,
    progress_sender: mpsc::Sender<SendStatus>,
    tokio_handle: TokioHandle,
) -> anyhow::Result<SendHandle> {
    match start_http_send_internal(path, progress_sender.clone(), tokio_handle).await {
        Ok(handle) => Ok(handle),
        Err(e) => {
            progress_sender
                .send(SendStatus::Error(e.to_string()))
                .await
                .ok();
            Err(e)
        }
    }
}

/// Public entry point for receiving a file using a ticket.
pub async fn receive_file(ticket_str: String, progress_sender: mpsc::Sender<ReceiveStatus>) {
     // Create a unique temporary directory name based on the ticket's hash.
    let dir_name = match BlobTicket::from_str(&ticket_str) {
        Ok(ticket) => format!(".p2p-client-recv-{}", ticket.hash().to_hex()),
        Err(e) => {
            progress_sender
                .send(ReceiveStatus::Error(e.to_string()))
                .await
                .ok();
            return;
        }
    };
    let data_dir = match std::env::current_dir() {
        Ok(dir) => dir.join(dir_name),
        Err(e) => {
            progress_sender
                .send(ReceiveStatus::Error(e.to_string()))
                .await
                .ok();
            return;
        }
    };

    let result =
        async { receive_logic(&ticket_str, &data_dir, progress_sender.clone()).await }.await;

    println!("Cleaning up temporary receive directory...");
    if let Err(e) = tokio::fs::remove_dir_all(&data_dir).await {
        println!("Failed to clean up temp dir {:?}: {}", data_dir, e);
    }

    if let Err(e) = result {
        progress_sender
            .send(ReceiveStatus::Error(e.to_string()))
            .await
            .ok();
    }
}
/// Core logic for P2P send.
async fn send_internal(
    path: PathBuf,
    progress: mpsc::Sender<SendStatus>,
    tokio_handle: TokioHandle,
) -> anyhow::Result<SendHandle> {
    progress.send(SendStatus::Connecting).await?;

    // 1. Setup Iroh endpoint with a new secret key.
    let secret_key = get_or_create_secret()?;
    let relay_mode = RelayMode::Default;
    let endpoint = Endpoint::builder()
        .alpns(vec![BlobsAlpn.to_vec()])
        .secret_key(secret_key)
        .relay_mode(relay_mode.clone())
        .bind()
        .await?;

    // 2. Create a temporary directory for the Iroh database.
    let suffix: [u8; 8] = rand::rng().random();
    let data_dir = std::env::temp_dir().join(format!("p2p-client-p2p-{}", hex::encode(suffix)));
    tokio::fs::create_dir_all(&data_dir).await?;
    let store = iroh_blobs::store::fs::FsStore::load(&data_dir).await?;

    // 3. Import the file/folder into the Iroh store and get the collection hash.
    let (temp_tag, _size, _collection) = import(&path, &store, progress.clone()).await?;

    // 4. Start listening for incoming connections.
    let blobs = BlobsProtocol::new(&store, None);
    let router = iroh::protocol::Router::builder(endpoint)
        .accept(BlobsAlpn, blobs)
        .spawn();

    // 5. Ensure we are connected to the relay network.
    let ep = router.endpoint();
    tokio::time::timeout(std::time::Duration::from_secs(10), async move {
        if !matches!(relay_mode, RelayMode::Disabled) {
            let _ = ep.online().await;
        }
    })
    .await?;

    // 6. Create the ticket and send it to the UI.
    let addr = router.endpoint().addr();
    let ticket = BlobTicket::new(addr, temp_tag.hash(), BlobFormat::HashSeq);
    progress
        .send(SendStatus::ReadyToSend {
            ticket: ticket.to_string(),
        })
        .await?;

    // 7. Create a shutdown channel to stop the task when the SendHandle is dropped.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = shutdown_rx.await;
        println!("Shutting down P2P sender...");
        let _ = router.shutdown().await;
        let _ = temp_tag;
    });

    // 8. Return the handle to the caller.
    Ok(SendHandle {
        data_dir,
        shutdown_tx: Some(shutdown_tx),
        _ngrok_tunnel: None,
        tokio_handle,
    })
}

/// Core logic for HTTP send.
async fn start_http_send_internal(
    path: PathBuf,
    progress_sender: mpsc::Sender<SendStatus>,
    tokio_handle: TokioHandle,
) -> anyhow::Result<SendHandle> {
    progress_sender.send(SendStatus::Connecting).await?;

    // 1. Create a temporary data directory and import the file.
    let suffix: [u8; 8] = rand::rng().random();
    let data_dir = std::env::temp_dir().join(format!("p2p-client-http-{}", hex::encode(suffix)));
    tokio::fs::create_dir_all(&data_dir).await?;
    let db = iroh_blobs::store::fs::FsStore::load(&data_dir).await?;
    let (_temp_tag, _size, collection) = import(&path, &db, progress_sender.clone()).await?;

    // Web send currently only supports single files.
    let (download_hash, file_name) = if collection.len() == 1 {
        let (name, hash) = collection.iter().next().context("Collection is empty")?;
        (*hash, name.clone())
    } else {
        bail!(
            "Sending directories via web link is not yet supported. Please select a single file."
        );
    };

    // 2. Set up the Axum web server.
    let app_state = AppState {
        db: Arc::new(db.into()),
        file_name,
    };

    let app = Router::new()
        .route("/download/{hash}", get(download_handler))
        .with_state(app_state);

    progress_sender.send(SendStatus::Connecting).await?;

    // Bind to a random available port on localhost.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let local_addr = listener.local_addr()?;

    // Create a shutdown channel for the server.
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

    // 3. Set up the ngrok tunnel.
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

    // 4. Send the public URL back to the UI.
    progress_sender
        .send(SendStatus::ReadyToSend { ticket: url })
        .await?;

    // 5. Return the handle.
    Ok(SendHandle {
        data_dir,
        shutdown_tx: Some(shutdown_tx),
        _ngrok_tunnel: Some(tun),
        tokio_handle,
    })
}

/// Core logic for receiving files.
async fn receive_logic(
    ticket_str: &str,
    data_dir: &Path,
    progress: mpsc::Sender<ReceiveStatus>,
) -> anyhow::Result<()> {
    progress.send(ReceiveStatus::Connecting).await?;

    // 1. Parse ticket and set up local Iroh endpoint.
    let ticket = BlobTicket::from_str(ticket_str).context("Invalid ticket format")?;
    let addr = ticket.addr().clone();
    let secret_key = get_or_create_secret()?;
    let endpoint = Endpoint::builder()
        .alpns(vec![])
        .secret_key(secret_key)
        .relay_mode(RelayMode::Default)
        .bind()
        .await?;

    let db = iroh_blobs::store::fs::FsStore::load(data_dir).await?;

    // 2. Connect to the sender and download the blob data.
    let hash_and_format = ticket.hash_and_format();
    let local = db.remote().local(hash_and_format).await?;
    if !local.is_complete() {
        let connection = endpoint.connect(addr, BlobsAlpn).await?;
        let (_hash_seq, sizes) =
            get_hash_seq_and_sizes(&connection, &hash_and_format.hash, 1024 * 1024 * 32, None)
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let total_size = sizes.iter().copied().sum::<u64>();
        let payload_size = sizes.iter().skip(1).copied().sum::<u64>();
        let total_files = (sizes.len().saturating_sub(1)) as u64;
        progress
            .send(ReceiveStatus::Connected {
                total_files,
                total_size: payload_size,
            })
            .await?;
        let get = db.remote().execute_get(connection, local.missing());
        let mut stream = get.stream();
        while let Some(item) = stream.next().await {
            match item {
                GetProgressItem::Progress(offset) => {
                    progress
                        .send(ReceiveStatus::Downloading {
                            downloaded: local.local_bytes() + offset,
                            total: total_size,
                        })
                        .await?;
                }
                GetProgressItem::Done(_) => break,
                GetProgressItem::Error(cause) => bail!(cause.to_string()),
            }
        }
    }

    // 3. Once download is complete, export files from the Iroh store to the filesystem.
    let collection = Collection::load(hash_and_format.hash, db.as_ref()).await?;
    export(&db, collection, progress.clone()).await?;

    db.shutdown().await?;

    progress.send(ReceiveStatus::Done).await?;
    Ok(())
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

/// Generates a new random secret key for an Iroh endpoint
fn get_or_create_secret() -> anyhow::Result<SecretKey> {
    Ok(SecretKey::generate(&mut rand::rng()))
}

/// Walks the given path, imports all files into the Iroh store, and creates a "collection".
/// A collection is a single hash that represents a group of files.
async fn import(
    path: &Path,
    db: &Store,
    progress: mpsc::Sender<SendStatus>,
) -> anyhow::Result<(TempTag, u64, Collection)> {
    let path = path.canonicalize()?;
    anyhow::ensure!(path.exists(), "path {} does not exist", path.display());
    let root = path.parent().context("Failed to get parent directory")?;
    let data_sources: Vec<(String, PathBuf)> = WalkDir::new(&path)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|entry| {
            let path = entry.into_path();
            let relative = path.strip_prefix(root)?.to_path_buf();
            let name = canonicalized_path_to_string(relative, true)?;
            Ok((name, path))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let total_files = data_sources.len();
    let total_size = data_sources
        .iter()
        .map(|(_, p)| p.metadata().map(|m| m.len()).unwrap_or(0))
        .sum();
    let mut names_and_tags = Vec::new();
    let mut done_files = 0;
    let mut done_size = 0;
    for (name, path) in data_sources {
        progress
            .send(SendStatus::Importing {
                total_files,
                done_files,
                total_size,
                done_size,
            })
            .await?;
        let import = db.add_path_with_opts(AddPathOptions {
            path,
            mode: ImportMode::TryReference,
            format: BlobFormat::Raw,
        });
        let mut stream = import.stream().await;
        let mut item_size = 0;
        let temp_tag = loop {
            match stream
                .next()
                .await
                .context("import stream ended unexpectedly")?
            {
                AddProgressItem::Size(size) => item_size = size,
                AddProgressItem::Done(tt) => break tt,
                AddProgressItem::Error(cause) => bail!("error importing {}: {}", name, cause),
                _ => {}
            }
        };
        done_files += 1;
        done_size += item_size;
        names_and_tags.push((name, temp_tag, item_size));
    }
    names_and_tags.sort_by(|(a, _, _), (b, _, _)| a.cmp(b));
    let size = names_and_tags.iter().map(|(_, _, size)| *size).sum::<u64>();
    let (collection, tags) = names_and_tags
        .into_iter()
        .map(|(name, tag, _)| ((name, tag.hash()), tag))
        .unzip::<_, _, Collection, Vec<_>>();
    let temp_tag = collection.clone().store(db).await?;
    drop(tags);
    Ok((temp_tag, size, collection))
}

/// Exports files from an Iroh collection to the local filesystem.
async fn export(
    db: &Store,
    collection: Collection,
    progress: mpsc::Sender<ReceiveStatus>,
) -> anyhow::Result<()> {
    let root = std::env::current_dir()?;
    let total_files = collection.len() as u64;
    for (i, (name, hash)) in collection.iter().enumerate() {
        progress
            .send(ReceiveStatus::Exporting {
                total_files,
                done_files: i as u64,
            })
            .await?;
        let target = get_export_path(&root, name)?;
        if target.exists() {
            bail!(
                "target {} already exists. Please remove it and try again.",
                target.display()
            );
        }
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut stream = db
            .export_with_opts(ExportOptions {
                hash: *hash,
                target,
                mode: ExportMode::Copy,
            })
            .stream()
            .await;
        while let Some(item) = stream.next().await {
            if let ExportProgressItem::Error(cause) = item {
                bail!("error exporting {}: {}", name, cause);
            }
        }
    }
    progress.send(ReceiveStatus::Done).await?;
    Ok(())
}

/// Safely constructs a valid export path from a root directory and a relative file name.
/// Prevents path traversal attacks (e.g., names like "../../../../etc/passwd").
fn get_export_path(root: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let mut path = root.to_path_buf();
    for part in name.split('/') {
        anyhow::ensure!(
            !part.contains('\\') && part != ".." && part != ".",
            "invalid path component: {}",
            part
        );
        path.push(part);
    }
    Ok(path)
}

/// Converts a Path to a string using forward slashes, ensuring it's safe.
fn canonicalized_path_to_string(
    path: impl AsRef<Path>,
    must_be_relative: bool,
) -> anyhow::Result<String> {
    let mut parts = Vec::new();
    for c in path.as_ref().components() {
        match c {
            Component::Normal(x) => {
                let part = x.to_str().context("Invalid characters in path")?;
                anyhow::ensure!(
                    !part.contains('/') && !part.contains('\\'),
                    "Invalid path component: {}",
                    part
                );
                parts.push(part);
            }
            Component::RootDir if !must_be_relative => {}
            _ => bail!("Invalid path component: {:?}", c),
        }
    }
    Ok(parts.join("/"))
}
