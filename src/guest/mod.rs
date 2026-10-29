//! A passthrough guest as a third clipboard, next to Wayland and X11.
//!
//! The guest's clipboard follows the keyboard and mouse: when kvm-switch's
//! router hands them to the guest, the latest copy on the host goes there;
//! when they come back, the guest's clipboard is fetched if something was
//! copied there meanwhile, and published on both displays. The Windows agent
//! (`guest/` in this repository) connects over vsock; `wire` holds the
//! messages. Copied files travel with their contents and are offered from a
//! directory of their own on the other side.

mod vsock;
// Some of it only the Windows agent uses.
#[allow(dead_code)]
pub mod wire;

use crate::payload::{self, GNOME, URI};
use crate::protocol::{Event, FILE_LIMIT, Payload, ReadItem};
use anyhow::{Context, Result};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

/// For an agent that stops reading: each write must make progress.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const HTML: &str = "text/html";
const PNG: &str = "image/png";

pub struct Config {
    /// The vsock port the agent connects to, and the only CID accepted.
    pub port: u32,
    pub cid: u32,
    /// kvm-switch-router's control socket, which reports `target host|guest`.
    pub switch_socket: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Host,
    Guest,
}

/// The host clipboard in the formats the agent sets.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outgoing {
    pub text: Vec<u8>,
    pub html: Vec<u8>,
    pub png: Vec<u8>,
    /// Local paths of copied files and folders.
    pub paths: Vec<String>,
}

enum Request {
    Push(Outgoing),
    Fetch,
}

/// Sends to the agent from a thread of its own, in order.
pub struct Guest {
    requests: mpsc::Sender<Request>,
}

impl Guest {
    pub fn push(&self, outgoing: Outgoing) {
        let _ = self.requests.send(Request::Push(outgoing));
    }

    pub fn fetch(&self) {
        let _ = self.requests.send(Request::Fetch);
    }
}

/// The connection in use, by id, for the writer.
type Current = Arc<Mutex<Option<(u64, File)>>>;

pub fn spawn(config: Config, events: mpsc::Sender<Event>) -> Result<Guest> {
    // Files from an earlier run are no longer on any clipboard.
    let _ = fs::remove_dir_all(cache_root());
    let listener = vsock::Listener::bind(config.port)
        .with_context(|| format!("bind vsock port {}", config.port))?;
    let current = Current::default();
    let (requests, receiver) = mpsc::channel();

    let accepted = Arc::clone(&current);
    let sender = events.clone();
    let cid = config.cid;
    thread::Builder::new()
        .name("guest-listen".into())
        .spawn(move || {
            let mut id = 0_u64;
            loop {
                let (stream, peer) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) => {
                        eprintln!("clipboard-bridge: guest accept: {error}");
                        thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                };
                if peer != cid {
                    eprintln!("clipboard-bridge: rejected vsock peer cid {peer}");
                    continue;
                }
                let Ok(reader) = stream.try_clone() else {
                    continue;
                };
                id += 1;
                let _ = vsock::set_write_timeout(&stream, WRITE_TIMEOUT);
                if let Some((_, old)) = accepted.lock().unwrap().replace((id, stream)) {
                    vsock::shutdown(&old);
                }
                eprintln!("clipboard-bridge: guest agent connected");
                let current = Arc::clone(&accepted);
                let events = sender.clone();
                let _ = thread::Builder::new()
                    .name("guest-read".into())
                    .spawn(move || read(id, reader, &current, &events));
            }
        })?;

    let path = config.switch_socket;
    thread::Builder::new()
        .name("guest-switch".into())
        .spawn(move || follow(&path, &events))?;

    thread::Builder::new()
        .name("guest-write".into())
        .spawn(move || {
            for request in receiver {
                write(&current, request);
            }
        })?;
    Ok(Guest { requests })
}

/// Reports the router's `target` lines, reconnecting while it is not running.
fn follow(path: &Path, events: &mpsc::Sender<Event>) {
    loop {
        if let Ok(stream) = UnixStream::connect(path) {
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                let target = match line.strip_prefix("target ") {
                    Some("host") => Target::Host,
                    Some("guest") => Target::Guest,
                    _ => continue,
                };
                if events.send(Event::Switch(Some(target))).is_err() {
                    return;
                }
            }
            if events.send(Event::Switch(None)).is_err() {
                return;
            }
        }
        thread::sleep(Duration::from_secs(2));
    }
}

fn write(current: &Current, request: Request) {
    let Some((id, mut stream)) = current
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|(id, stream)| Some((*id, stream.try_clone().ok()?)))
    else {
        return;
    };
    let result = match request {
        Request::Fetch => stream.write_all(b"get\n"),
        Request::Push(outgoing) => {
            let Some(clip) = outgoing_clip(outgoing) else {
                return;
            };
            let header = clip.header("set");
            let sent = std::iter::once(header.as_bytes())
                .chain(clip.parts())
                .try_for_each(|part| stream.write_all(part));
            if sent.is_ok() {
                eprintln!(
                    "clipboard-bridge: -> guest ({} B text, {} B html, {} B png, {} B files)",
                    clip.text.len(),
                    clip.html.len(),
                    clip.png.len(),
                    clip.files.len()
                );
            }
            sent
        }
    };
    if result.is_err() {
        eprintln!("clipboard-bridge: writing to the guest agent failed");
        vsock::shutdown(&stream);
        let mut current = current.lock().unwrap();
        if current.as_ref().is_some_and(|(c, _)| *c == id) {
            *current = None;
        }
    }
}

/// Reads the copied files from disk; None, and nothing sent, over the limit.
fn outgoing_clip(outgoing: Outgoing) -> Option<wire::Clip> {
    let Outgoing {
        text,
        html,
        png,
        paths,
    } = outgoing;
    let files = if paths.is_empty() {
        Vec::new()
    } else if let Some(files) = wire::read_files(&paths) {
        files
    } else {
        eprintln!(
            "clipboard-bridge: copied files exceed {} MiB, not sent to the guest",
            wire::FILES_LIMIT >> 20
        );
        return None;
    };
    Some(wire::Clip {
        text,
        html,
        png,
        files,
    })
}

/// The agent's replies until it disconnects; a changed clipboard becomes a
/// copy on the host.
fn read(id: u64, stream: File, current: &Current, events: &mpsc::Sender<Event>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if line.trim() == "same" {
            continue;
        }
        let Some([text, html, png, files]) = wire::parse_header(&line, "clip") else {
            // Its data, if any, would be read as lines.
            eprintln!("clipboard-bridge: bad guest reply {:?}", line.trim());
            break;
        };
        let mut clip = wire::Clip {
            text: vec![0; text],
            html: vec![0; html],
            png: vec![0; png],
            files: vec![0; files],
        };
        let complete = [
            &mut clip.text,
            &mut clip.html,
            &mut clip.png,
            &mut clip.files,
        ]
        .into_iter()
        .try_for_each(|part| reader.read_exact(part));
        if complete.is_err() {
            break;
        }
        let summary = format!(
            "{} B text, {} B html, {} B png",
            clip.text.len(),
            clip.html.len(),
            clip.png.len()
        );
        match incoming(clip) {
            Some((payload, files)) => {
                eprintln!("clipboard-bridge: <- guest ({summary}, {files} files)");
                if events.send(Event::GuestOffer(payload)).is_err() {
                    return;
                }
            }
            None => eprintln!("clipboard-bridge: <- guest changed, but sent nothing to set"),
        }
    }
    let mut current = current.lock().unwrap();
    if current.as_ref().is_some_and(|(c, _)| *c == id) {
        *current = None;
        eprintln!("clipboard-bridge: guest agent disconnected");
    }
}

fn cache_root() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("clipboard-bridge")
}

/// The guest's clipboard in every host format it maps to, and the number of
/// files written; None when nothing can be set.
fn incoming(clip: wire::Clip) -> Option<(Payload, usize)> {
    let wire::Clip {
        text,
        html,
        png,
        files,
    } = clip;
    let paths: Vec<String> = match wire::unpack(&files) {
        Some(entries) if !entries.is_empty() => {
            wire::write_files(&wire::fresh_dir(&cache_root()), &entries)
                .into_iter()
                .filter_map(|path| path.into_os_string().into_string().ok())
                .collect()
        }
        Some(_) => Vec::new(),
        None => {
            eprintln!("clipboard-bridge: malformed files from the guest");
            Vec::new()
        }
    };
    drop(files);
    // Like a file manager: copied files also paste as their paths.
    let text = if text.is_empty() {
        paths.join("\n").into_bytes()
    } else {
        String::from_utf8_lossy(&text).into_owned().into_bytes()
    };
    let mut formats = Vec::new();
    if !text.is_empty() {
        formats.extend(payload::text_formats(Arc::new(text))?);
    }
    if !html.is_empty() {
        formats.push((HTML.to_owned(), Arc::new(html)));
    }
    if !png.is_empty() {
        formats.push((PNG.to_owned(), Arc::new(png)));
    }
    if !paths.is_empty() {
        let uris: Vec<String> = paths.iter().map(|path| wire::file_uri(path)).collect();
        let list = Arc::new(uris.join("\r\n").into_bytes());
        formats.extend(payload::files(URI, list, None)?.formats);
    }
    let files = paths.len();
    (!formats.is_empty()).then_some((Payload { formats }, files))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Text,
    Html,
    Png,
    Files,
}

/// The parts of a host offer that go to the guest, read together.
#[derive(Debug)]
pub struct Bundle {
    parts: Vec<(Part, String)>,
}

impl Bundle {
    pub fn select(types: &[String]) -> Option<Self> {
        let offered = |mime: &str| types.iter().any(|t| t == mime);
        // Copied files go as files alone; their text is just the paths.
        if let Some(mime) = [GNOME, URI].into_iter().find(|mime| offered(mime)) {
            return Some(Self {
                parts: vec![(Part::Files, mime.to_owned())],
            });
        }
        let mut parts = Vec::new();
        if let Some(mime) = payload::TEXT.into_iter().find(|mime| offered(mime)) {
            parts.push((Part::Text, mime.to_owned()));
        }
        // As between the displays, an image wins over its HTML companion,
        // which would only reference it.
        if offered(PNG) {
            parts.push((Part::Png, PNG.to_owned()));
        } else if offered(HTML) {
            parts.push((Part::Html, HTML.to_owned()));
        }
        (!parts.is_empty()).then_some(Self { parts })
    }

    pub fn requests(&self) -> Vec<ReadItem> {
        self.parts
            .iter()
            .map(|(part, mime)| ReadItem {
                mime: mime.clone(),
                limit: if *part == Part::Files {
                    FILE_LIMIT
                } else {
                    wire::LIMIT
                },
            })
            .collect()
    }

    pub fn normalize(&self, values: Vec<Arc<Vec<u8>>>) -> Option<Outgoing> {
        if values.len() != self.parts.len() {
            return None;
        }
        let mut outgoing = Outgoing::default();
        for ((part, mime), value) in self.parts.iter().zip(values) {
            match part {
                Part::Text if mime == "STRING" => {
                    outgoing.text = value
                        .iter()
                        .map(|&b| char::from(b))
                        .collect::<String>()
                        .into_bytes();
                }
                Part::Text => {
                    outgoing.text = String::from_utf8_lossy(&value).into_owned().into_bytes()
                }
                Part::Html => outgoing.html = wire::utf8_html(&value),
                Part::Png => outgoing.png = value.to_vec(),
                Part::Files => {
                    let list = String::from_utf8_lossy(&value);
                    let list = if mime == GNOME {
                        list.split_once('\n').map_or("", |(_, uris)| uris)
                    } else {
                        &list
                    };
                    outgoing.paths = wire::uri_paths(list);
                }
            }
        }
        (outgoing != Outgoing::default()).then_some(outgoing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(list: &[&str]) -> Vec<String> {
        list.iter().map(|&t| t.to_owned()).collect()
    }

    #[test]
    fn files_go_alone_and_an_image_wins_over_html() {
        let bundle = Bundle::select(&types(&["text/plain", URI, PNG])).unwrap();
        assert_eq!(bundle.parts, vec![(Part::Files, URI.to_owned())]);
        let bundle = Bundle::select(&types(&["UTF8_STRING", HTML, PNG])).unwrap();
        assert_eq!(
            bundle.parts,
            vec![
                (Part::Text, "UTF8_STRING".to_owned()),
                (Part::Png, PNG.to_owned())
            ]
        );
        let bundle = Bundle::select(&types(&["text/plain;charset=utf-8", HTML])).unwrap();
        assert_eq!(bundle.parts[1], (Part::Html, HTML.to_owned()));
        assert!(Bundle::select(&types(&["application/x-other"])).is_none());
    }

    #[test]
    fn bundles_normalize_to_what_the_agent_sets() {
        let bundle = Bundle::select(&types(&["STRING", HTML])).unwrap();
        let out = bundle
            .normalize(vec![
                Arc::new(b"caf\xe9".to_vec()),
                Arc::new(b"\xef\xbb\xbf<b>x</b>".to_vec()),
            ])
            .unwrap();
        assert_eq!(out.text, "café".as_bytes());
        assert_eq!(out.html, b"<b>x</b>");
        let bundle = Bundle::select(&types(&[GNOME])).unwrap();
        let out = bundle
            .normalize(vec![Arc::new(
                b"cut\nfile:///tmp/a%20b\nfile:///tmp/c".to_vec(),
            )])
            .unwrap();
        assert_eq!(out.paths, vec!["/tmp/a b".to_owned(), "/tmp/c".to_owned()]);
        assert!(bundle.normalize(Vec::new()).is_none());
    }

    #[test]
    fn guest_clipboards_map_to_every_host_format() {
        let (payload, files) = incoming(wire::Clip {
            text: b"hi".to_vec(),
            html: b"<i>hi</i>".to_vec(),
            ..wire::Clip::default()
        })
        .unwrap();
        assert_eq!(files, 0);
        assert_eq!(payload.data("UTF8_STRING").unwrap().as_ref(), b"hi");
        assert_eq!(payload.data(HTML).unwrap().as_ref(), b"<i>hi</i>");
        assert!(payload.data(URI).is_none());
        assert!(incoming(wire::Clip::default()).is_none());
    }
}
