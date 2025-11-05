use ngrok::forwarder::Forwarder;
use std::path::PathBuf;
use tokio::runtime::Handle as TokioHandle;
use ngrok::tunnel::TunnelCloser;

/// Defines the states of a send operation for reporting progress to the UI.
#[derive(Debug, Clone)]
pub enum SendStatus {
    Connecting,
    Importing { total_files: usize, done_files: usize, total_size: u64, done_size: u64 },
    ReadyToSend { ticket: String },
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
    pub(crate) data_dir: PathBuf,
    pub(crate) shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    pub(crate) _ngrok_tunnel: Option<Forwarder<ngrok::tunnel::HttpTunnel>>,
    pub(crate) tokio_handle: TokioHandle,
}
/// The Drop implementation ensures that background tasks are shut down and temporary files are deleted.
impl Drop for SendHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(mut tunnel) = self._ngrok_tunnel.take() {
            self.tokio_handle.spawn(async move {
                // Тепер .close() буде доступний
                let _ = tunnel.close().await;
                println!("Ngrok tunnel closed.");
            });
        }
        let data_dir = self.data_dir.clone();
        std::thread::spawn(move || {
            let _ = std::fs::remove_dir_all(data_dir);
        });
        println!("Send operation cancelled and cleaning up.");
    }
}