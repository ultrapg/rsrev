use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::net::TcpStream;

pub const CHUNK_SIZE: usize = 65536;
pub const MAX_MSG_SIZE: usize = 10 * 1024 * 1024; // 10 MB

#[derive(Debug, PartialEq)]
pub enum ParseError {
    NeedMore,
    Invalid(String),
}

// ── JSON control messages ──

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Message {
    // Shell lifecycle
    StartShell,
    StopShell,
    ShellClosed,
    // Shell I/O
    CmdInput(String),
    CmdOutput(String),
    // File transfer
    DownloadRequest(String),
    UploadRequest(String),
    FileReady,
    FileDone,
    FileError(String),
    // Directory listing
    ListDir(String),
    DirEntries(Vec<DirEntry>),
    // General
    Success,
    Error(String),
    Exit,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: String,
}

// ── Raw binary chunk (no JSON overhead) ──

pub struct FileChunk {
    pub total_size: u64,
    pub offset: u64,
    pub is_last: bool,
    pub data: Vec<u8>,
}

// ── Unified frame type ──

pub enum Frame {
    Msg(Message),
    Chunk(FileChunk),
}

// ── Wire format helpers ──

const FT_MSG: u8 = 0x00;
const FT_CHUNK: u8 = 0x01;

/// Try to parse one frame from a byte slice (non-consuming).
pub fn parse_frame(data: &[u8]) -> Result<(Frame, usize), ParseError> {
    if data.is_empty() {
        return Err(ParseError::NeedMore);
    }
    match data[0] {
        FT_MSG => {
            if data.len() < 5 {
                return Err(ParseError::NeedMore);
            }
            let len = u32::from_le_bytes(data[1..5].try_into().unwrap()) as usize;
            if len > MAX_MSG_SIZE {
                return Err(ParseError::Invalid(format!("message too large: {} bytes", len)));
            }
            if data.len() < 5 + len {
                return Err(ParseError::NeedMore);
            }
            let msg: Message = serde_json::from_slice(&data[5..5 + len])
                .map_err(|e| ParseError::Invalid(e.to_string()))?;
            Ok((Frame::Msg(msg), 5 + len))
        }
        FT_CHUNK => {
            if data.len() < 22 {
                return Err(ParseError::NeedMore);
            }
            let total_size = u64::from_le_bytes(data[1..9].try_into().unwrap());
            let offset = u64::from_le_bytes(data[9..17].try_into().unwrap());
            let is_last = data[17] != 0;
            let dlen = u32::from_le_bytes(data[18..22].try_into().unwrap()) as usize;
            if data.len() < 22 + dlen {
                return Err(ParseError::NeedMore);
            }
            let chunk = FileChunk {
                total_size,
                offset,
                is_last,
                data: data[22..22 + dlen].to_vec(),
            };
            Ok((Frame::Chunk(chunk), 22 + dlen))
        }
        b => Err(ParseError::Invalid(format!("bad frame tag 0x{:02x}", b))),
    }
}

/// Read one frame from the stream (blocking).
pub fn read_frame(stream: &mut TcpStream) -> io::Result<Frame> {
    let mut tag = [0u8; 1];
    stream.read_exact(&mut tag)?;
    match tag[0] {
        FT_MSG => {
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf)?;
            let len = u32::from_le_bytes(len_buf) as usize;
            if len > MAX_MSG_SIZE {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("message too large: {} bytes", len)));
            }
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf)?;
            let msg: Message = serde_json::from_slice(&buf)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(Frame::Msg(msg))
        }
        FT_CHUNK => {
            let mut hdr = [0u8; 21];
            stream.read_exact(&mut hdr)?;
            let total_size = u64::from_le_bytes(hdr[0..8].try_into().unwrap());
            let offset = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
            let is_last = hdr[16] != 0;
            let dlen = u32::from_le_bytes(hdr[17..21].try_into().unwrap()) as usize;
            let mut data = vec![0u8; dlen];
            stream.read_exact(&mut data)?;
            Ok(Frame::Chunk(FileChunk { total_size, offset, is_last, data }))
        }
        b => Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad frame tag 0x{:02x}", b))),
    }
}

/// Write a JSON control message.
pub fn write_msg(stream: &mut TcpStream, msg: &Message) -> io::Result<()> {
    let json = serde_json::to_vec(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = json.len() as u32;
    stream.write_all(&[FT_MSG])?;
    stream.write_all(&len.to_le_bytes())?;
    stream.write_all(&json)?;
    stream.flush()?;
    Ok(())
}

/// Write a raw binary file chunk.
pub fn write_chunk(stream: &mut TcpStream, chunk: &FileChunk) -> io::Result<()> {
    let dlen = chunk.data.len() as u32;
    stream.write_all(&[FT_CHUNK])?;
    stream.write_all(&chunk.total_size.to_le_bytes())?;
    stream.write_all(&chunk.offset.to_le_bytes())?;
    stream.write_all(&[chunk.is_last as u8])?;
    stream.write_all(&dlen.to_le_bytes())?;
    stream.write_all(&chunk.data)?;
    stream.flush()?;
    Ok(())
}
