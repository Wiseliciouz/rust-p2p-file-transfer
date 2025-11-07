use eframe::{egui, App, Frame};
use egui::{
    Align, Button, CentralPanel, Color32, Context, Frame as EguiFrame, Layout, ProgressBar,
    RichText, Stroke,
};
use p2p_client::{receive_file, send_file, start_http_send, ReceiveStatus, SendHandle, SendStatus};
use rfd::FileDialog;
use rustls::crypto::CryptoProvider;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

struct MyApp {
    // --- UI State ---
    ticket_input: String,           // Text field for the received ticket.
    path_to_send: Option<PathBuf>,  // Path of the file/folder selected for sending.
    status_message: String,         // Displays current status or errors.
    progress_value: f32,            // Progress bar value (0.0 to 1.0).
    is_drag_hover: bool,            // True if a file is being dragged over the window.
    is_web_send_active: bool,       // True if a web (ngrok) transfer is active.

    // --- Async Communication ---
    send_progress_rx: Option<mpsc::Receiver<SendStatus>>, // Receives status updates for sending.
    receive_progress_rx: Option<mpsc::Receiver<ReceiveStatus>>, // Receives status updates for receiving.
    tokio_rt: Arc<Runtime>, // The Tokio runtime to execute async tasks.

    // --- Transfer Management ---
    send_handle_rx: Option<mpsc::Receiver<anyhow::Result<SendHandle>>>, // Receives the handle to manage a send operation.
    send_handle: Option<SendHandle>, // Holds the handle for the *currently starting* send operation.
    active_sends: Vec<(String, SendHandle, SendType)>, // List of active background transfers.
}

// Differentiates between a P2P ticket transfer and a web link transfer.
#[derive(Clone, Copy, PartialEq)]
enum SendType {
    P2P,
    Web,
}

impl MyApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self {
            ticket_input: String::new(),
            path_to_send: None,
            status_message: "Ready to work".to_string(),
            send_progress_rx: None,
            receive_progress_rx: None,
            tokio_rt: Arc::new(Runtime::new().expect("Failed to create Tokio runtime")),
            send_handle_rx: None,
            send_handle: None,
            active_sends: Vec::new(),
            progress_value: 0.0,
            is_drag_hover: false,
            is_web_send_active: false,
        }
    }

    fn update_web_send_status(&mut self) {
        self.is_web_send_active = self
            .active_sends
            .iter()
            .any(|(_, _, send_type)| *send_type == SendType::Web);
    }

    fn handle_progress_updates(&mut self) {
        if let Some(ref mut rx) = self.send_handle_rx {
            if let Ok(handle_result) = rx.try_recv() {
                match handle_result {
                    Ok(handle) => self.send_handle = Some(handle),
                    Err(e) => {
                        self.status_message = format!("Error during startup: {}", e);
                        self.send_handle = None;
                    }
                }
                self.send_handle_rx = None;
            }
        }

        // Process status updates for the sending operation.
        if let Some(ref mut rx) = self.send_progress_rx {
            if let Ok(status) = rx.try_recv() {
                match status {
                    SendStatus::Connecting => {
                        self.status_message = "Connection...".to_string();
                    }
                    SendStatus::Importing {
                        done_files,
                        total_files,
                        ..
                    } => {
                        self.status_message =
                            format!("Importing files: {} / {}", done_files, total_files);
                        self.progress_value = if total_files > 0 {
                            done_files as f32 / total_files as f32
                        } else {
                            0.0
                        };
                    }
                    SendStatus::ReadyToSend { ticket } => {
                        self.status_message = format!("Done! Click to copy:\n{}", ticket);
                        self.progress_value = 0.0;
                        if let Some(handle) = self.send_handle.take() {
                            let send_type = if ticket.starts_with("http") {
                                SendType::Web
                            } else {
                                SendType::P2P
                            };
                            self.active_sends.push((ticket.clone(), handle, send_type));
                            self.update_web_send_status();
                        }
                        self.send_progress_rx = None;
                        self.path_to_send = None;
                    }
                    SendStatus::Error(e) => {
                        self.status_message = format!("Error: {}", e);
                        self.reset_send_state();
                    }
                    SendStatus::Done => {
                        self.status_message = "The transfer has been canceled..".to_string();
                        self.reset_send_state();
                    }
                }
            }
        }

        // Process status updates for the receiving operation.
        if let Some(ref mut rx) = self.receive_progress_rx {
            if let Ok(status) = rx.try_recv() {
                match status {
                    ReceiveStatus::Connecting => {
                        self.status_message = "Connection...".to_string();
                        self.progress_value = 0.0;
                    }
                    ReceiveStatus::Connected {
                        total_files,
                        total_size,
                    } => {
                        self.status_message = format!(
                            "Obtaining metadata: {} files, {}",
                            total_files,
                            bytesize::ByteSize(total_size)
                        );
                    }
                    ReceiveStatus::Downloading { downloaded, total } => {
                        self.status_message = format!(
                            "Download: {} / {}",
                            bytesize::ByteSize(downloaded),
                            bytesize::ByteSize(total)
                        );
                        self.progress_value = if total > 0 {
                            downloaded as f32 / total as f32
                        } else {
                            0.0
                        };
                    }
                    ReceiveStatus::Exporting {
                        done_files,
                        total_files,
                    } => {
                        self.status_message = format!("Download: {} / {}", done_files, total_files);
                    }
                    ReceiveStatus::Done => {
                        self.status_message = "Download complete!".to_string();
                        self.receive_progress_rx = None;
                        self.progress_value = 0.0;
                    }
                    ReceiveStatus::Error(e) => {
                        self.status_message = format!("Download error: {}", e);
                        self.receive_progress_rx = None;
                        self.progress_value = 0.0;
                    }
                }
            }
        }
    }

    // Resets the state related to sending a file.
    fn reset_send_state(&mut self) {
        self.send_progress_rx = None;
        self.send_handle = None;
        self.send_handle_rx = None;
        self.progress_value = 0.0;
        self.path_to_send = None;
    }

    // Handles file drag and drop events.
    fn handle_drag_and_drop(&mut self, ctx: &Context) {
        // Check if files are being hovered over the window.
        self.is_drag_hover = !ctx.input(|i| i.raw.hovered_files.is_empty());

        // Check if files were dropped.
        if !ctx.input(|i| i.raw.dropped_files.is_empty()) {
            if let Some(path) =
                ctx.input(|i| i.raw.dropped_files.first().and_then(|f| f.path.clone()))
            {
                self.path_to_send = Some(path);
            }
        }
    }
}

// Implement the `eframe::App` trait for our struct to make it a runnable GUI app.
impl App for MyApp {
    fn update(&mut self, ctx: &Context, _frame: &mut Frame) {
        self.handle_progress_updates();
        self.handle_drag_and_drop(ctx);

        CentralPanel::default().show(ctx, |ui| {
            ui.with_layout(Layout::top_down(Align::Center), |ui| {
                ui.strong(RichText::new("Send").size(30.0));
            });

            egui::Frame::new()
                .outer_margin(egui::Margin::symmetric(110, 0))
                .show(ui, |ui| {
                    let mut frame = EguiFrame::canvas(ui.style());
                    if self.is_drag_hover {
                        frame.fill = Color32::from_rgb(50, 50, 60);
                        frame.stroke = Stroke::new(2.0, Color32::from_rgb(0, 150, 255));
                    }

                    let response = frame
                        .show(ui, |ui| {
                            ui.with_layout(Layout::top_down(Align::Center), |ui| {
                                if self.path_to_send.is_none() {
                                    ui.add_space(10.0);
                                    ui.strong(RichText::new("Drag & drop").size(30.0));
                                    ui.add_space(5.0);
                                    ui.label(RichText::new("or browse to select").size(12.0));
                                    if ui
                                        .add(
                                            Button::new(
                                                RichText::new("files")
                                                    .size(12.0)
                                                    .color(ui.visuals().hyperlink_color),
                                            )
                                            .frame(false),
                                        )
                                        .clicked()
                                    {
                                        if let Some(path) = FileDialog::new().pick_file() {
                                            self.path_to_send = Some(path);
                                        }
                                    }
                                    ui.label(RichText::new("or").size(12.0));
                                    if ui
                                        .add(
                                            Button::new(
                                                RichText::new("folders")
                                                    .size(12.0)
                                                    .color(ui.visuals().hyperlink_color),
                                            )
                                            .frame(false),
                                        )
                                        .clicked()
                                    {
                                        if let Some(path) = FileDialog::new().pick_folder() {
                                            self.path_to_send = Some(path);
                                        }
                                    };
                                    ui.add_space(10.0);
                                } else {
                                    ui.add_space(10.0);
                                    ui.label("Сhosen path:");
                                    ui.strong(
                                        self.path_to_send.as_ref().unwrap().to_string_lossy(),
                                    );
                                    ui.add_space(8.0);

                                    if self.send_handle.is_none() && self.send_handle_rx.is_none() {
                                        ui.with_layout(Layout::top_down(Align::Center), |ui| {
                                            if ui.button("Send (ticket)").clicked() {
                                                let path = self.path_to_send.clone().unwrap();
                                                let (progress_tx, progress_rx) = mpsc::channel(10);
                                                let (handle_tx, handle_rx) = mpsc::channel(1);
                                                self.send_progress_rx = Some(progress_rx);
                                                self.send_handle_rx = Some(handle_rx);
                                                let rt = self.tokio_rt.clone();
                                                let tokio_handle = rt.handle().clone();
                                                rt.spawn(async move {
                                                    let handle_result =
                                                        send_file(path, progress_tx, tokio_handle)
                                                            .await;
                                                    let _ = handle_tx.send(handle_result).await;
                                                });
                                            }

                                            let web_button_enabled = !self.is_web_send_active;
                                            let web_button_tooltip = if web_button_enabled {
                                                ""
                                            } else {
                                                "Only one webcast at a time on the free plan"
                                            };

                                            if ui
                                                .add_enabled(
                                                    web_button_enabled,
                                                    Button::new("Send (web)"),
                                                )
                                                .on_disabled_hover_text(web_button_tooltip)
                                                .clicked()
                                            {
                                                let path = self.path_to_send.clone().unwrap();
                                                let (progress_tx, progress_rx) = mpsc::channel(10);
                                                let (handle_tx, handle_rx) = mpsc::channel(1);
                                                self.send_progress_rx = Some(progress_rx);
                                                self.send_handle_rx = Some(handle_rx);
                                                let rt = self.tokio_rt.clone();
                                                let tokio_handle = rt.handle().clone();
                                                rt.spawn(async move {
                                                    let handle_result = start_http_send(
                                                        path,
                                                        progress_tx,
                                                        tokio_handle,
                                                    )
                                                    .await;
                                                    let _ = handle_tx.send(handle_result).await;
                                                });
                                            }
                                            if ui.button("Cancel").clicked() {
                                                self.path_to_send = None;
                                            }
                                        });
                                    }

                                    if let Some(_handle) = &self.send_handle {
                                        if ui.button("Cancel sending").clicked() {
                                            self.reset_send_state();
                                        }
                                    }
                                    ui.add_space(10.0);
                                }
                            });
                        })
                        .response;

                    if self.path_to_send.is_none() && response.clicked() {
                        if let Some(path) = FileDialog::new().pick_file() {
                            self.path_to_send = Some(path);
                        }
                    }
                });

            ui.separator();

            ui.with_layout(Layout::top_down(Align::Center), |ui| {
                ui.strong(RichText::new("Receive (ticket)").size(30.0));
            });
            ui.label(egui::RichText::new("ticket:").size(15.0));
            let text_edit_widget = egui::TextEdit::multiline(&mut self.ticket_input)
                .font(egui::FontId::proportional(15.0))
                .hint_text("Insert your ticket here...");

            let desired_width = ui.available_width();
            let desired_height = 80.0;

            ui.add_sized([desired_width, desired_height], text_edit_widget);
            let get_button = egui::Button::new(egui::RichText::new("Get").size(18.0));

            let button_width = 60.0;
            let button_height = 40.0;

            if ui
                .add_sized([button_width, button_height], get_button)
                .clicked()
            {
                self.status_message = "Starting download...".to_string();
                let rt = self.tokio_rt.clone();
                let ticket = self.ticket_input.clone();
                let (tx, rx) = mpsc::channel(32);
                self.receive_progress_rx = Some(rx);
                rt.spawn(async move { receive_file(ticket, tx).await });
            }

            ui.separator();
            ui.with_layout(Layout::top_down(Align::Center), |ui| {
                ui.strong(RichText::new("Status").size(30.0));
            });
            if self.progress_value > 0.0 {
                ui.add(ProgressBar::new(self.progress_value).show_percentage());
            }

            if let Some(ticket) = self.status_message.strip_prefix("Done! Click to copy:\n") {
                ui.label("Done! Click to copy:");
                if ui.button(ticket.trim()).clicked() {
                    ctx.copy_text(ticket.trim().to_string());
                }
            } else {
                ui.label(&self.status_message);
            }

            ui.separator();
            ui.with_layout(Layout::top_down(Align::Center), |ui| {
                ui.strong(RichText::new("Active background transfers").size(30.0));
            });

            if self.active_sends.is_empty() {
                ui.label("Empty.");
            }

            let mut changed = false;
            self.active_sends.retain(|(ticket, _handle, _send_type)| {
                let mut keep = true;
                ui.horizontal(|ui| {
                    let display_ticket = if ticket.len() > 60 {
                        format!("{}...", &ticket[..60])
                    } else {
                        ticket.to_string()
                    };
                    ui.label(&display_ticket);
                    if ui.button("Stop").clicked() {
                        self.status_message = "Background transmission stopped.".to_string();
                        keep = false;
                        changed = true;
                    }
                });
                keep
            });

            if changed {
                self.update_web_send_status();
            }

            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        });
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    // Initialize the crypto provider for secure connections.
    let _ = CryptoProvider::install_default(rustls::crypto::ring::default_provider());
    let native_options = eframe::NativeOptions::default();
    // Run the eframe application.
    eframe::run_native(
        "transfers GUI",
        native_options,
        Box::new(|cc| Ok(Box::new(MyApp::new(cc)))),
    )?;
    Ok(())
}
