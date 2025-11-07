# rust-p2p-file-transfer
Peer-to-peer (P2P) file transfer application built with Rust.
![Demo ticked](./ticked.gif)
![Demo web](./web.gif)
## About The Project

This application allows users to establish a direct connection and transfer files using an intuitive desktop interface, bypassing centralized servers for the data transfer itself. The project consists of a full-featured desktop application (`p2p-client`) that users run to send and receive files.

It also includes a revolutionary feature to generate a public web link, allowing anyone to download a file directly from the user's computer via a web browser.

## Key Features

-   **Intuitive Desktop GUI**: A user-friendly graphical interface for managing transfers with drag-and-drop support.
-   **Direct File Transfer**: Files are transferred directly from one peer to another using **Iroh tickets**, ensuring privacy and speed.
-   **Universal Web Link Transfer**: Generate a public URL to share a file with anyone, no special software required for the recipient.
-   **Automatic NAT Traversal**: Utilizes `Iroh`'s capabilities to establish connections between peers behind most routers.
-   **Cross-Platform**: Built to run on Windows, macOS, and Linux.

## Technology Stack

### General
-   **Language**: `Rust`
-   **Asynchronous Runtime**: `tokio`
-   **Serialization**: `serde`

### P2P Client (Desktop App)
-   **GUI Framework**: `egui` - For building the cross-platform graphical user interface.
-   **P2P Networking**: `Iroh` - The core engine for peer discovery, connection management, and data transfer.
-   **Web Link Server**: `axum` & `ngrok` - For serving files over a public URL.
-   **Native File Dialogs**: `rfd` (Rust File Dialog) - For system-native "Open File" windows.

## Roadmap
-   [+] **MVP**:
    -   [+] Implement a basic signaling mechanism. *(Achieved using Iroh's public relay network instead of a custom server).*
    -   [+] Set up the basic desktop application window using `egui`.
    -   [+] Implement core P2P logic for connecting and discovering other peers.
    -   [+] Establish a P2P connection between two clients on a local network.
    -   [+] Implement the transfer of a file, initiated from the GUI.
-   [+] **Phase 2 - Features & Reliability**:
    -   [+] Improve NAT Traversal mechanisms (e.g., hole punching, relays).
    -   [+] Implement reliable large file transfer (chunking, progress tracking).
    -   [+] Refine the user interface and user experience (drag & drop, background transfers).
