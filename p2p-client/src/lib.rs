#![allow(clippy::large_enum_variant)]
mod files;
mod p2p;
mod state;
mod web;

pub use state::{ReceiveStatus, SendHandle, SendStatus};

use iroh_blobs::ticket::BlobTicket;
use std::path::PathBuf;
use std::str::FromStr;
use tokio::{runtime::Handle as TokioHandle, sync::mpsc};

/// Public entry point for starting a P2P (ticket-based) send operation.
pub async fn send_file(
    path: PathBuf,
    progress_sender: mpsc::Sender<SendStatus>,
    tokio_handle: TokioHandle,
) -> anyhow::Result<SendHandle> {
    p2p::send_internal(path, progress_sender, tokio_handle).await
}

/// Public entry point for starting an HTTP (web link) send operation.
pub async fn start_http_send(
    path: PathBuf,
    progress_sender: mpsc::Sender<SendStatus>,
    tokio_handle: TokioHandle,
) -> anyhow::Result<SendHandle> {
    // Викликаємо функцію з модуля web
    web::start_http_send_internal(path, progress_sender, tokio_handle).await
}

/// Public entry point for receiving a file using a ticket.
pub async fn receive_file(ticket_str: String, progress_sender: mpsc::Sender<ReceiveStatus>) {
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
        async { p2p::receive_logic(&ticket_str, &data_dir, progress_sender.clone()).await }.await;

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