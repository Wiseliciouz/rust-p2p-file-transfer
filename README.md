# rust-p2p-file-transfer
Peer-to-peer (P2P) file transfer application built with Rust and libp2p.

## About The Project

This application allows users to establish a direct connection and transfer files using an intuitive desktop interface, bypassing centralized servers for the data transfer itself. The project consists of two main components:

1.  **Signal Server (`signal-server`)**: A lightweight intermediary server that helps clients "find" each other on the network (Peer Discovery) and establish a direct connection.
2.  **P2P Client (`p2p-client`)**: A full-featured desktop application that users run to send and receive files through a graphical interface.

## Key Features

-   **Intuitive Desktop GUI**: A user-friendly graphical interface for managing transfers and viewing online peers.
-   **Direct File Transfer**: Files are transferred directly from one peer to another, ensuring privacy and speed.
-   **Automatic NAT Traversal**: Utilizes `libp2p`'s capabilities to establish connections between peers behind most routers.
-   **Peer Discovery**: Easily see which other users are currently online and available for transfers.
-   **Cross-Platform**: Built to run on Windows, macOS, and Linux.

## Technology Stack

### General
-   **Language**: `Rust`
-   **Asynchronous Runtime**: `tokio`
-   **Serialization**: `serde`
-   **Logging**: `tracing`

### P2P Client (Desktop App)
-   **GUI Framework**: `egui` - For building the cross-platform graphical user interface.
-   **P2P Networking**: `libp2p-rs` - The core engine for peer discovery, connection management, and data transfer.
-   **Native File Dialogs**: `rfd` (Rust File Dialog) - For system-native "Open File" windows.

### Signal Server
-   **Web Framework**: `axum` - For handling HTTP and WebSocket connections from clients.
-   **Real-time Messaging**: `WebSockets` (via `axum`) - For instant communication during the peer connection setup.

## Roadmap
-   [ ] **MVP**:
    -   [ ] Implement the basic signaling server using `axum` + `WebSocket`.
    -   [ ] Set up the basic desktop application window using `egui`.
    -   [ ] Implement core `libp2p` logic for connecting to the signal server and discovering other peers.
    -   [ ] Establish a P2P connection between two clients on a local network.
    -   [ ] Implement the transfer of a file, initiated from the GUI.
-   [ ] **Phase 2 - Features & Reliability**:
    -   [ ] Improve NAT Traversal mechanisms (e.g., hole punching, relays).
    -   [ ] Implement reliable large file transfer (chunking, progress tracking, resuming).
    -   [ ] Refine the user interface and user experience (e.g., transfer history, settings).
