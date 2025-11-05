use super::files::{export, import};
use super::state::{ReceiveStatus, SendHandle, SendStatus};
use anyhow::{bail, Context};
use iroh::{Endpoint, RelayMode, SecretKey};
use iroh_blobs::{
    api::remote::GetProgressItem,
    format::collection::Collection,
    get::request::get_hash_seq_and_sizes,
    protocol::ALPN as BlobsAlpn,
    ticket::BlobTicket,
    BlobFormat, BlobsProtocol,
};
use n0_future::StreamExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tokio::{runtime::Handle as TokioHandle, sync::mpsc};

/// Core logic for P2P send.
pub(crate) async fn send_internal(
    path: PathBuf,
    progress: mpsc::Sender<SendStatus>,
    tokio_handle: TokioHandle,
) -> anyhow::Result<SendHandle> {
    progress.send(SendStatus::Connecting).await?;

    let secret_key = get_or_create_secret()?;
    let relay_mode = RelayMode::Default;
    let endpoint = Endpoint::builder()
        .alpns(vec![BlobsAlpn.to_vec()])
        .secret_key(secret_key)
        .relay_mode(relay_mode.clone())
        .bind()
        .await?;

    let suffix: [u8; 8] = rand::random();
    let data_dir = std::env::temp_dir().join(format!("p2p-client-p2p-{}", hex::encode(suffix)));
    tokio::fs::create_dir_all(&data_dir).await?;
    let store = iroh_blobs::store::fs::FsStore::load(&data_dir).await?;

    let (temp_tag, _size, _collection) = import(&path, &store, progress.clone()).await?;

    let blobs = BlobsProtocol::new(&store, None);
    let router = iroh::protocol::Router::builder(endpoint)
        .accept(BlobsAlpn, blobs)
        .spawn();

    let ep = router.endpoint();
    tokio::time::timeout(std::time::Duration::from_secs(10), async move {
        if !matches!(relay_mode, RelayMode::Disabled) {
            let _ = ep.online().await;
        }
    })
    .await?;

    let addr = router.endpoint().addr();
    let ticket = BlobTicket::new(addr, temp_tag.hash(), BlobFormat::HashSeq);
    progress
        .send(SendStatus::ReadyToSend {
            ticket: ticket.to_string(),
        })
        .await?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = shutdown_rx.await;
        println!("Shutting down P2P sender...");
        let _ = router.shutdown().await;
        let _ = temp_tag;
    });

    Ok(SendHandle {
        data_dir,
        shutdown_tx: Some(shutdown_tx),
        _ngrok_tunnel: None,
        tokio_handle,
    })
}

/// Core logic for receiving files.
pub(crate) async fn receive_logic(
    ticket_str: &str,
    data_dir: &Path,
    progress: mpsc::Sender<ReceiveStatus>,
) -> anyhow::Result<()> {
    progress.send(ReceiveStatus::Connecting).await?;

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

    let collection = Collection::load(hash_and_format.hash, db.as_ref()).await?;
    export(&db, collection, progress.clone()).await?;

    db.shutdown().await?;

    progress.send(ReceiveStatus::Done).await?;
    Ok(())
}

/// Generates a new random secret key for an Iroh endpoint
fn get_or_create_secret() -> anyhow::Result<SecretKey> {
    Ok(SecretKey::generate(&mut rand::rng()))
}