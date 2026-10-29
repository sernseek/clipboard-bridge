//! clipboard-bridge's Windows guest agent: the guest end of the bridge's
//! guest side (`src/guest` in this repository), over vsock to the host
//! (CID 2).
//!
//!   clipboard-bridge-guest.exe [port]     (default 8789)
//!
//! The host sends its clipboard when the input comes here and asks for this
//! one when the input leaves. Text, HTML, images and files: an image is read
//! from the "PNG" format or a bitmap and set as both; HTML is Windows' "HTML
//! Format"; copied files (CF_HDROP) go across with their contents, and files
//! from the host are written to %TEMP%\clipboard-bridge and put on the
//! clipboard from there.
//!
//! Runs in the user's session (a logon task). Needs the viosock driver, which
//! registers AF_VSOCK (40) as a Winsock provider. It supports only blocking
//! calls, so the connection lives on its own thread; the clipboard is read
//! and set on a message-only window's thread, since the owner window must
//! answer clipboard messages.

#![windows_subsystem = "windows"]

mod dib;
/// The messages, file packing and HTML conversion, shared with the host.
#[path = "../../src/guest/wire.rs"]
#[allow(dead_code)]
mod wire;

use std::{
    cell::Cell,
    mem::{size_of, zeroed},
    path::PathBuf,
    ptr::{null, null_mut},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{GlobalFree, HANDLE, HWND, LPARAM, LRESULT, WPARAM},
    Networking::WinSock::{
        INVALID_SOCKET, SOCK_STREAM, SOCKADDR, SOCKET, WSADATA, WSAStartup, closesocket, connect,
        recv, send, socket,
    },
    System::{
        DataExchange::{
            CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
            IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
        },
        LibraryLoader::GetModuleHandleW,
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
        Ole::{CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT},
    },
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG,
        RegisterClassW, SendMessageW, WM_APP, WNDCLASSW,
    },
};

const AF_VSOCK: u16 = 40;
const VMADDR_CID_HOST: u32 = 2;

/// struct sockaddr_vm
#[repr(C)]
struct SockaddrVm {
    family: u16,
    reserved: u16,
    port: u32,
    cid: u32,
    zero: [u8; 4],
}

/// The message-only window, for the connection thread to send requests to.
static WINDOW: AtomicUsize = AtomicUsize::new(0);
/// A clipboard handed between the connection thread and the window.
static SLOT: Mutex<Option<Board>> = Mutex::new(None);

/// Read the clipboard into SLOT; returns 0 when unchanged since the last sync.
const WM_CLIP_GET: u32 = WM_APP + 1;
/// Set the clipboard from SLOT.
const WM_CLIP_SET: u32 = WM_APP + 2;

thread_local! {
    /// Clipboard sequence number after the last sync in either direction.
    static SYNCED: Cell<u32> = const { Cell::new(0) };
}

/// The clipboard as the window's thread reads and sets it: UTF-8 text with
/// `\n` line ends, HTML, PNG, and copied files by path; empty when absent.
#[derive(Default)]
struct Board {
    text: Vec<u8>,
    html: Vec<u8>,
    png: Vec<u8>,
    files: Vec<PathBuf>,
}

/// Where files from the host are written.
fn files_root() -> PathBuf {
    std::env::temp_dir().join("clipboard-bridge")
}

/// Connects to `port` on the host and serves the connection, again whenever
/// it ends.
fn reconnect(port: u32) -> ! {
    loop {
        let s = unsafe { socket(i32::from(AF_VSOCK), SOCK_STREAM, 0) };
        if s != INVALID_SOCKET {
            let addr = SockaddrVm {
                family: AF_VSOCK,
                reserved: 0,
                port,
                cid: VMADDR_CID_HOST,
                zero: [0; 4],
            };
            let ok = unsafe {
                connect(
                    s,
                    (&addr as *const SockaddrVm).cast::<SOCKADDR>(),
                    size_of::<SockaddrVm>() as i32,
                )
            } == 0;
            if ok {
                serve(s);
            }
            unsafe { closesocket(s) };
        }
        thread::sleep(Duration::from_secs(3));
    }
}

/// Buffered reads from a blocking socket.
struct Reader {
    s: SOCKET,
    buf: Vec<u8>,
}

impl Reader {
    fn fill(&mut self) -> Option<()> {
        let mut chunk = vec![0u8; 64 << 10];
        let n = unsafe { recv(self.s, chunk.as_mut_ptr(), chunk.len() as i32, 0) };
        (n > 0).then(|| self.buf.extend_from_slice(&chunk[..n as usize]))
    }

    fn line(&mut self) -> Option<String> {
        loop {
            if let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=nl).collect();
                return Some(String::from_utf8_lossy(&line).into_owned());
            }
            if self.buf.len() > 4096 {
                return None;
            }
            self.fill()?;
        }
    }

    /// What is buffered, then the rest received straight into place.
    fn exact(&mut self, n: usize) -> Option<Vec<u8>> {
        let buffered = self.buf.len().min(n);
        let mut out: Vec<u8> = self.buf.drain(..buffered).collect();
        out.resize(n, 0);
        let mut have = buffered;
        while have < n {
            let chunk = (n - have).min(1 << 20);
            let got = unsafe { recv(self.s, out[have..].as_mut_ptr(), chunk as i32, 0) };
            if got <= 0 {
                return None;
            }
            have += got as usize;
        }
        Some(out)
    }
}

fn send_all(s: SOCKET, mut data: &[u8]) -> Option<()> {
    while !data.is_empty() {
        let chunk = data.len().min(1 << 20);
        let n = unsafe { send(s, data.as_ptr(), chunk as i32, 0) };
        if n <= 0 {
            return None;
        }
        data = &data[n as usize..];
    }
    Some(())
}

/// Serves the host's `get` and `set` (see `wire`); the clipboard itself is
/// touched on the window's thread.
fn serve(s: SOCKET) {
    let mut r = Reader { s, buf: Vec::new() };
    let window = WINDOW.load(Ordering::SeqCst) as HWND;
    while let Some(line) = r.line() {
        match line.split_whitespace().next() {
            Some("get") => {
                let changed = unsafe { SendMessageW(window, WM_CLIP_GET, 0, 0) } != 0;
                let sent = match SLOT.lock().unwrap().take() {
                    // The files are read here, off the window's thread.
                    Some(b) if changed => {
                        let clip = wire::Clip {
                            files: wire::read_files(&b.files).unwrap_or_default(),
                            text: b.text,
                            html: b.html,
                            png: b.png,
                        };
                        send_all(s, clip.header("clip").as_bytes()).and_then(|()| {
                            clip.parts()
                                .into_iter()
                                .try_for_each(|part| send_all(s, part))
                        })
                    }
                    _ => send_all(s, b"same\n"),
                };
                if sent.is_none() {
                    return;
                }
            }
            Some("set") => {
                let Some([t, h, p, f]) = wire::parse_header(&line, "set") else {
                    return;
                };
                let (Some(text), Some(html), Some(png), Some(packed)) =
                    (r.exact(t), r.exact(h), r.exact(p), r.exact(f))
                else {
                    return;
                };
                let files = match wire::unpack(&packed) {
                    Some(entries) if !entries.is_empty() => {
                        wire::write_files(&wire::fresh_dir(&files_root()), &entries)
                    }
                    _ => Vec::new(),
                };
                drop(packed);
                *SLOT.lock().unwrap() = Some(Board {
                    text,
                    html,
                    png,
                    files,
                });
                unsafe { SendMessageW(window, WM_CLIP_SET, 0, 0) };
            }
            _ => {}
        }
    }
}

/// Opens the clipboard, waiting a little for whoever has it open.
fn open_clipboard(window: HWND) -> bool {
    for _ in 0..25 {
        if unsafe { OpenClipboard(window) } != 0 {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

/// A clipboard format's data; the clipboard must be open.
fn clipboard_bytes(format: u32) -> Option<Vec<u8>> {
    unsafe {
        if IsClipboardFormatAvailable(format) == 0 {
            return None;
        }
        let handle = GetClipboardData(format);
        if handle.is_null() {
            return None;
        }
        let size = GlobalSize(handle);
        let data = GlobalLock(handle);
        if data.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(data.cast::<u8>(), size).to_vec();
        GlobalUnlock(handle);
        Some(bytes)
    }
}

/// Puts a copy of `bytes` on the open clipboard.
fn set_clipboard_bytes(format: u32, bytes: &[u8]) {
    unsafe {
        let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
        if handle.is_null() {
            return;
        }
        let data = GlobalLock(handle);
        if data.is_null() {
            GlobalFree(handle);
            return;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast::<u8>(), bytes.len());
        GlobalUnlock(handle);
        if SetClipboardData(format, handle as HANDLE).is_null() {
            GlobalFree(handle);
        }
    }
}

fn registered(name: &str) -> u32 {
    let name: Vec<u16> = name.encode_utf16().chain([0]).collect();
    unsafe { RegisterClipboardFormatW(name.as_ptr()) }
}

/// The paths in a DROPFILES (CF_HDROP).
fn dropped_files(drop: &[u8]) -> Vec<PathBuf> {
    let u32_at = |at: usize| {
        drop.get(at..at + 4)
            .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    let names = drop.get(u32_at(0) as usize..).unwrap_or_default();
    let names: Vec<String> = if u32_at(16) != 0 {
        let wide: Vec<u16> = names
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_le_bytes(pair))
            .collect();
        wide.split(|&c| c == 0)
            .take_while(|n| !n.is_empty())
            .map(String::from_utf16_lossy)
            .collect()
    } else {
        names
            .split(|&c| c == 0)
            .take_while(|n| !n.is_empty())
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .collect()
    };
    names.into_iter().map(PathBuf::from).collect()
}

/// A wide DROPFILES for `paths`.
fn drop_files(paths: &[PathBuf]) -> Vec<u8> {
    // pFiles, pt.x, pt.y, fNC, fWide
    let mut out: Vec<u8> = [20u32, 0, 0, 0, 1]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    for p in paths {
        let p = p.to_string_lossy();
        out.extend(p.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
    }
    out.extend([0, 0]);
    out
}

/// WM_CLIP_GET: the clipboard into SLOT, unless nothing was copied since the
/// last sync.
fn clip_get(window: HWND) -> bool {
    if unsafe { GetClipboardSequenceNumber() } == SYNCED.get() || !open_clipboard(window) {
        return false;
    }
    let mut clip = Board::default();
    if let Some(wide) = clipboard_bytes(u32::from(CF_UNICODETEXT)) {
        let wide: Vec<u16> = wide
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_le_bytes(pair))
            .take_while(|&c| c != 0)
            .collect();
        clip.text = String::from_utf16_lossy(&wide)
            .replace("\r\n", "\n")
            .into_bytes();
    }
    clip.html = clipboard_bytes(registered("HTML Format"))
        .and_then(|cf| wire::from_cf_html(&cf))
        .unwrap_or_default();
    clip.png = clipboard_bytes(registered("PNG"))
        .or_else(|| clipboard_bytes(registered("image/png")))
        .or_else(|| clipboard_bytes(u32::from(CF_DIBV5)).and_then(|d| dib::to_png(&d)))
        .or_else(|| clipboard_bytes(u32::from(CF_DIB)).and_then(|d| dib::to_png(&d)))
        .unwrap_or_default();
    if let Some(drop) = clipboard_bytes(u32::from(CF_HDROP)) {
        clip.files = dropped_files(&drop);
    }
    unsafe { CloseClipboard() };
    SYNCED.set(unsafe { GetClipboardSequenceNumber() });
    for part in [&mut clip.text, &mut clip.html, &mut clip.png] {
        if part.len() > wire::LIMIT {
            part.clear();
        }
    }
    *SLOT.lock().unwrap() = Some(clip);
    true
}

/// WM_CLIP_SET: SLOT onto the clipboard, the image as PNG and a bitmap,
/// files as a copy (not a cut).
fn clip_set(window: HWND) {
    let Some(clip) = SLOT.lock().unwrap().take() else {
        return;
    };
    if !open_clipboard(window) {
        return;
    }
    unsafe { EmptyClipboard() };
    if !clip.text.is_empty() {
        let text = String::from_utf8_lossy(&clip.text)
            .replace("\r\n", "\n")
            .replace('\n', "\r\n");
        let wide: Vec<u8> = text
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        set_clipboard_bytes(u32::from(CF_UNICODETEXT), &wide);
    }
    if !clip.html.is_empty() {
        set_clipboard_bytes(registered("HTML Format"), &wire::cf_html(&clip.html));
    }
    if !clip.png.is_empty() {
        set_clipboard_bytes(registered("PNG"), &clip.png);
        if let Some(dib) = dib::from_png(&clip.png) {
            set_clipboard_bytes(u32::from(CF_DIB), &dib);
        }
    }
    if !clip.files.is_empty() {
        set_clipboard_bytes(u32::from(CF_HDROP), &drop_files(&clip.files));
        // DROPEFFECT_COPY
        set_clipboard_bytes(registered("Preferred DropEffect"), &1u32.to_le_bytes());
    }
    unsafe { CloseClipboard() };
    SYNCED.set(unsafe { GetClipboardSequenceNumber() });
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CLIP_GET => LRESULT::from(clip_get(hwnd)),
        WM_CLIP_SET => {
            clip_set(hwnd);
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn main() {
    let port = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(8789);
    unsafe {
        let mut wsa: WSADATA = zeroed();
        if WSAStartup(0x0202, &mut wsa) != 0 {
            return;
        }
        let class: Vec<u16> = "clipboard-bridge-guest\0".encode_utf16().collect();
        let instance = GetModuleHandleW(null());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            return;
        }
        // What is already on the clipboard stays here until copied again.
        SYNCED.set(GetClipboardSequenceNumber());
        let _ = std::fs::remove_dir_all(files_root());
        WINDOW.store(hwnd as usize, Ordering::SeqCst);
        thread::spawn(move || reconnect(port));
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            DispatchMessageW(&msg);
        }
    }
}
