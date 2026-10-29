//! Clipboard messages between the bridge's guest side and the Windows guest
//! agent (vsock), shared with the guest crate. A clipboard is UTF-8 text with
//! `\n` line ends, an HTML document, a PNG image and/or files; an empty field
//! means the format is absent.
//!
//!   host  -> guest: `get`, `set T H P F` + T bytes of text, H of HTML, P of
//!                   PNG, F of files
//!   guest -> host:  `same` (unchanged since the last sync), `clip T H P F` + data
//!
//! Files travel with their contents (`read_files`): the other side writes
//! them to a directory of its own and offers those copies. Only std here, so
//! the guest crate builds this file too.

use std::{
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    io::Read,
    path::{Path, PathBuf},
};

/// Text, HTML and PNG each; larger ones are not synced.
pub const LIMIT: usize = 64 << 20;
/// All files of one copy together, as packed.
pub const FILES_LIMIT: usize = 512 << 20;

#[derive(Clone, Default, PartialEq, Eq, Debug, Hash)]
pub struct Clip {
    pub text: Vec<u8>,
    /// UTF-8, a whole document or a fragment.
    pub html: Vec<u8>,
    pub png: Vec<u8>,
    /// Packed entries (`read_files`, `unpack`).
    pub files: Vec<u8>,
}

impl Clip {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.html.is_empty() && self.png.is_empty() && self.files.is_empty()
    }

    /// `verb T H P F\n`, followed on the wire by `parts`.
    pub fn header(&self, verb: &str) -> String {
        format!(
            "{verb} {} {} {} {}\n",
            self.text.len(),
            self.html.len(),
            self.png.len(),
            self.files.len()
        )
    }

    /// The data after the header, sent as is rather than copied into one
    /// buffer.
    pub fn parts(&self) -> [&[u8]; 4] {
        [&self.text, &self.html, &self.png, &self.files]
    }
}

pub fn digest(value: impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}

/// The data lengths of a `verb T H P F` line, within the limits.
pub fn parse_header(line: &str, verb: &str) -> Option<[usize; 4]> {
    let mut words = line.split_whitespace();
    if words.next() != Some(verb) {
        return None;
    }
    let mut len = |limit| {
        words
            .next()
            .and_then(|w| w.parse::<usize>().ok())
            .filter(|&n| n <= limit)
    };
    Some([len(LIMIT)?, len(LIMIT)?, len(LIMIT)?, len(FILES_LIMIT)?])
}

/// A file or directory, by its path relative to the copied items' parent:
/// the first component is the item that was copied. Borrows from the
/// packed data.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Entry<'a> {
    Dir(&'a str),
    File(&'a str, &'a [u8]),
}

impl Entry<'_> {
    pub fn path(&self) -> &str {
        match self {
            Entry::Dir(p) | Entry::File(p, _) => p,
        }
    }
}

const DIR: u64 = u64::MAX;

/// An entry's path length (u32 LE), path and content length (u64 LE, all
/// ones for a directory); a file's content follows.
fn push_head(out: &mut Vec<u8>, path: &str, len: u64) {
    out.extend_from_slice(&(path.len() as u32).to_le_bytes());
    out.extend_from_slice(path.as_bytes());
    out.extend_from_slice(&len.to_le_bytes());
}

pub fn pack(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in entries {
        match e {
            Entry::Dir(p) => push_head(&mut out, p, DIR),
            Entry::File(p, data) => {
                push_head(&mut out, p, data.len() as u64);
                out.extend_from_slice(data);
            }
        }
    }
    out
}

/// None when malformed or a path could leave the target directory.
pub fn unpack(mut blob: &[u8]) -> Option<Vec<Entry<'_>>> {
    fn take<'a>(blob: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        let (head, rest) = blob.split_at_checked(n)?;
        *blob = rest;
        Some(head)
    }
    let mut entries = Vec::new();
    while !blob.is_empty() {
        let n = u32::from_le_bytes(take(&mut blob, 4)?.try_into().ok()?) as usize;
        let path = std::str::from_utf8(take(&mut blob, n)?).ok()?;
        if !safe_path(path) {
            return None;
        }
        let len = u64::from_le_bytes(take(&mut blob, 8)?.try_into().ok()?);
        entries.push(if len == DIR {
            Entry::Dir(path)
        } else {
            Entry::File(path, take(&mut blob, usize::try_from(len).ok()?)?)
        });
    }
    Some(entries)
}

/// Relative and `/`-separated. Names one system allows and the other does
/// not are fixed by `local_name` on the receiving side.
fn safe_path(path: &str) -> bool {
    !path.contains('\0')
        && path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

/// A name as Windows allows it: forbidden characters become `_`, trailing
/// dots and spaces go, and a device name (`CON`, `nul.txt`) gets a leading
/// `_`. Never empty, `.` or `..`.
pub fn windows_name(name: &str) -> String {
    const DEVICES: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let mut out: String = name
        .chars()
        .map(|c| {
            if c < ' ' || r#"<>:"/\|?*"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    out.truncate(out.trim_end_matches(['.', ' ']).len());
    let stem = out.split('.').next().unwrap_or_default().trim_end();
    if out.is_empty() || DEVICES.iter().any(|d| stem.eq_ignore_ascii_case(d)) {
        out.insert(0, '_');
    }
    out
}

/// A path component as this system names it.
fn local_name(name: &str) -> String {
    if cfg!(windows) {
        windows_name(name)
    } else {
        name.to_owned()
    }
}

/// The copied items: each entry's first component, in order.
pub fn top_level<'a>(entries: &[Entry<'a>]) -> Vec<&'a str> {
    let mut top: Vec<&str> = Vec::new();
    for e in entries {
        let (Entry::Dir(p) | Entry::File(p, _)) = e;
        let first = p.split('/').next().unwrap_or_default();
        if !top.contains(&first) {
            top.push(first);
        }
    }
    top
}

/// Packs `path` (as `rel`) and, for a directory, everything below it onto
/// `out`. Unreadable entries and symlinks below the top are left out; false
/// once `out` exceeds `FILES_LIMIT`.
fn collect(path: &Path, rel: String, out: &mut Vec<u8>) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return true;
    };
    if meta.is_dir() {
        push_head(out, &rel, DIR);
        let Ok(dir) = fs::read_dir(path) else {
            return out.len() <= FILES_LIMIT;
        };
        let mut children: Vec<_> = dir.flatten().collect();
        children.sort_by_key(|c| c.file_name());
        for child in children {
            let Ok(name) = child.file_name().into_string() else {
                continue;
            };
            if child.file_type().is_ok_and(|t| t.is_symlink()) {
                continue;
            }
            if !collect(&child.path(), format!("{rel}/{name}"), out) {
                return false;
            }
        }
    } else if meta.is_file() {
        let start = out.len();
        push_head(out, &rel, 0);
        let len_at = out.len() - 8;
        // Bounded: a file that grows past the limit while read stops there.
        let room = FILES_LIMIT.saturating_sub(out.len()) as u64;
        if meta.len() > room {
            return false;
        }
        match fs::File::open(path).and_then(|f| f.take(room + 1).read_to_end(out)) {
            Ok(n) => out[len_at..len_at + 8].copy_from_slice(&(n as u64).to_le_bytes()),
            Err(_) => out.truncate(start),
        }
    }
    out.len() <= FILES_LIMIT
}

/// The copied files and directories, packed straight from disk; None over
/// `FILES_LIMIT`.
pub fn read_files(paths: &[impl AsRef<Path>]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for path in paths {
        let path = path.as_ref();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !collect(path, name.to_owned(), &mut out) {
            return None;
        }
    }
    Some(out)
}

/// Writes the entries below `dir` and returns the copied items written.
pub fn write_files(dir: &Path, entries: &[Entry]) -> Vec<PathBuf> {
    let local = |path: &str| {
        path.split('/')
            .fold(dir.to_path_buf(), |p, c| p.join(local_name(c)))
    };
    for e in entries {
        let path = local(e.path());
        let _ = match e {
            Entry::Dir(_) => fs::create_dir_all(&path),
            Entry::File(_, data) => path
                .parent()
                .map_or(Ok(()), fs::create_dir_all)
                .and_then(|()| fs::write(&path, data)),
        };
    }
    let mut written: Vec<PathBuf> = Vec::new();
    for path in top_level(entries).into_iter().map(local) {
        // Two names can become one on Windows.
        if path.exists() && !written.contains(&path) {
            written.push(path);
        }
    }
    written
}

/// A fresh directory below `root` for one copy's files; the earlier ones
/// are no longer on any clipboard and are removed.
pub fn fresh_dir(root: &Path) -> PathBuf {
    let _ = fs::remove_dir_all(root);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    root.join(stamp.to_string())
}

/// `file://` URI of an absolute path, percent-encoding all but unreserved
/// characters and `/`.
pub fn file_uri(path: &str) -> String {
    let mut uri = String::from("file://");
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

/// The local paths of a `text/uri-list`.
pub fn uri_paths(list: &str) -> Vec<String> {
    list.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("file://"))
        // file://host/path: only the local host's.
        .filter_map(|l| l.strip_prefix("localhost").or(Some(l)))
        .filter(|l| l.starts_with('/'))
        .filter_map(|l| {
            let b = l.as_bytes();
            let mut out = Vec::with_capacity(b.len());
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'%' {
                    let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                    out.push(u8::from_str_radix(hex, 16).ok()?);
                    i += 3;
                } else {
                    out.push(b[i]);
                    i += 1;
                }
            }
            String::from_utf8(out).ok()
        })
        .collect()
}

/// `text/html` as UTF-8: some applications offer UTF-16 with a byte order
/// mark, others a UTF-8 one or a trailing NUL.
pub fn utf8_html(data: &[u8]) -> Vec<u8> {
    let utf16 = |rest: &[u8], le: bool| {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| {
                if le {
                    u16::from_le_bytes(pair)
                } else {
                    u16::from_be_bytes(pair)
                }
            })
            .collect();
        String::from_utf16_lossy(&units).into_bytes()
    };
    let mut html = match data {
        [0xff, 0xfe, rest @ ..] => utf16(rest, true),
        [0xfe, 0xff, rest @ ..] => utf16(rest, false),
        [0xef, 0xbb, 0xbf, rest @ ..] => rest.to_vec(),
        _ => data.to_vec(),
    };
    while html.last() == Some(&0) {
        html.pop();
    }
    match String::from_utf8(html) {
        Ok(text) => text.into_bytes(),
        Err(e) => String::from_utf8_lossy(e.as_bytes())
            .into_owned()
            .into_bytes(),
    }
}

const START_FRAGMENT: &str = "<!--StartFragment-->";
const END_FRAGMENT: &str = "<!--EndFragment-->";

/// The document in Windows' "HTML Format": a header of byte offsets, then
/// the HTML with the copied part between fragment comments. A document
/// keeps its own markup around the body; anything else is wrapped.
pub fn cf_html(html: &[u8]) -> Vec<u8> {
    let html = String::from_utf8_lossy(html);
    // ASCII lowercasing keeps every byte offset.
    let lower = html.to_ascii_lowercase();
    let marked = lower
        .find("<!--startfragment-->")
        .zip(lower.rfind("<!--endfragment-->"))
        .filter(|(start, end)| start + START_FRAGMENT.len() <= *end);
    let body = || {
        let open = lower.find("<body")?;
        let start = open + lower[open..].find('>')? + 1;
        let end = lower.rfind("</body")?;
        (start <= end).then_some((start, end))
    };
    let (document, start, end) = if let Some((start, end)) = marked {
        (html.into_owned(), start + START_FRAGMENT.len(), end)
    } else if let Some((start, end)) = body() {
        let document = format!(
            "{}{START_FRAGMENT}{}{END_FRAGMENT}{}",
            &html[..start],
            &html[start..end],
            &html[end..]
        );
        let shift = START_FRAGMENT.len();
        (document, start + shift, end + shift)
    } else {
        let prefix = format!("<html><body>\r\n{START_FRAGMENT}");
        let document = format!("{prefix}{html}{END_FRAGMENT}\r\n</body></html>");
        (document, prefix.len(), prefix.len() + html.len())
    };
    let header = |offsets: [usize; 4]| {
        format!(
            "Version:0.9\r\nStartHTML:{:010}\r\nEndHTML:{:010}\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n",
            offsets[0], offsets[1], offsets[2], offsets[3]
        )
    };
    let len = header([0; 4]).len();
    let mut out = header([len, len + document.len(), len + start, len + end]).into_bytes();
    out.extend_from_slice(document.as_bytes());
    out.push(0);
    out
}

/// The HTML in a Windows "HTML Format" clipboard: the whole document when
/// its offsets are given, else the fragment.
pub fn from_cf_html(data: &[u8]) -> Option<Vec<u8>> {
    let head = &data[..data.len().min(512)];
    let head = String::from_utf8_lossy(head);
    let offset = |key: &str| -> Option<usize> {
        let line = head
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))?;
        usize::try_from(line.trim().parse::<i64>().ok()?).ok()
    };
    let slice = |start: Option<usize>, end: Option<usize>| {
        let (start, end) = (start?, end?.min(data.len()));
        data.get(start..end).filter(|s| !s.is_empty())
    };
    let html = slice(offset("StartHTML"), offset("EndHTML"))
        .or_else(|| slice(offset("StartFragment"), offset("EndFragment")))?;
    Some(utf8_html(html))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let clip = Clip {
            text: b"hi\n".to_vec(),
            html: b"<b>".to_vec(),
            png: vec![0x89, b'P'],
            files: vec![7],
        };
        let header = clip.header("set");
        assert_eq!(parse_header(&header, "set"), Some([3, 3, 2, 1]));
        assert_eq!(clip.parts().concat(), b"hi\n<b>\x89P\x07");
    }

    #[test]
    fn headers_are_checked() {
        assert_eq!(parse_header("clip 1 2 0 0", "set"), None);
        assert_eq!(parse_header("clip 1 2 0", "clip"), None);
        assert_eq!(parse_header("clip 1 x 0 0", "clip"), None);
        assert_eq!(parse_header("clip 1 2 0 x", "clip"), None);
        assert_eq!(
            parse_header(&format!("clip 0 {} 0 0", LIMIT + 1), "clip"),
            None
        );
        assert_eq!(
            parse_header(&format!("clip 0 0 0 {}", LIMIT + 1), "clip"),
            Some([0, 0, 0, LIMIT + 1])
        );
        assert_eq!(
            parse_header(&format!("clip 0 0 0 {}", FILES_LIMIT + 1), "clip"),
            None
        );
        assert_eq!(parse_header("clip 0 0 0 0\n", "clip"), Some([0; 4]));
    }

    #[test]
    fn files_round_trip() {
        let entries = vec![
            Entry::File("a.png", &[1, 2, 3]),
            Entry::Dir("dir"),
            Entry::File("dir/文件 1.txt", &[]),
            Entry::Dir("dir/empty"),
        ];
        let blob = pack(&entries);
        assert_eq!(unpack(&blob), Some(entries.clone()));
        assert_eq!(top_level(&entries), vec!["a.png", "dir"]);
        assert_eq!(unpack(&blob[..blob.len() - 1]), None);
    }

    #[test]
    fn unsafe_paths_are_rejected() {
        for path in ["../x", "/etc/x", "a//b", "a/./b", "a/..", "a\0b", ""] {
            let blob = pack(&[Entry::Dir(path)]);
            assert_eq!(unpack(&blob), None, "{path:?}");
        }
        // Fine here, fixed by local_name on Windows.
        assert!(unpack(&pack(&[Entry::Dir("10:30 a\\b?")])).is_some());
    }

    #[test]
    fn windows_names() {
        for (name, fixed) in [
            ("10:30 <a>|b?.txt", "10_30 _a__b_.txt"),
            ("a\\b", "a_b"),
            ("x. .", "x"),
            ("..", "_"),
            (".", "_"),
            ("con", "_con"),
            ("Nul.txt", "_Nul.txt"),
            ("LPT1 .log", "_LPT1 .log"),
            ("console", "console"),
            ("tab\there", "tab_here"),
            (".bashrc", ".bashrc"),
        ] {
            assert_eq!(windows_name(name), fixed, "{name:?}");
        }
    }

    #[test]
    fn files_on_disk_round_trip() {
        let base =
            std::env::temp_dir().join(format!("clipboard-bridge-test-{}", std::process::id()));
        let src = base.join("src");
        fs::create_dir_all(src.join("d/empty")).unwrap();
        fs::write(src.join("a.txt"), "a").unwrap();
        fs::write(src.join("d/b.bin"), [0, 1]).unwrap();
        let blob = read_files(&[src.join("a.txt"), src.join("d")]).unwrap();
        let entries = unpack(&blob).unwrap();
        let out = base.join("out");
        let top = write_files(&out, &entries);
        assert_eq!(top, vec![out.join("a.txt"), out.join("d")]);
        assert_eq!(fs::read(out.join("d/b.bin")).unwrap(), vec![0, 1]);
        assert!(out.join("d/empty").is_dir());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_limit_counts_the_packing() {
        let base =
            std::env::temp_dir().join(format!("clipboard-bridge-limit-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        let file = base.join("exactly-the-limit");
        fs::File::create(&file)
            .unwrap()
            .set_len(FILES_LIMIT as u64)
            .unwrap();
        // The contents fit, the header on top does not.
        assert_eq!(read_files(&[&file]), None);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn uris_round_trip() {
        let path = "/home/u/图 片#1 %.png";
        let uri = file_uri(path);
        assert_eq!(uri.find(' '), None);
        let list = format!("# comment\r\n{uri}\r\nhttps://x/y\r\nfile://localhost/tmp/a\r\n");
        assert_eq!(uri_paths(&list), vec![path.to_string(), "/tmp/a".into()]);
    }

    #[test]
    fn html_documents_and_fragments_round_trip_through_cf_html() {
        for (html, fragment) in [
            ("<b>bold</b> 中文", "<b>bold</b> 中文"),
            (
                "<html><head><style>b{}</style></head><BODY class=x><i>it</i></BODY></html>",
                "<i>it</i>",
            ),
            (
                "<meta charset='utf-8'><html><body><!--StartFragment--><u>u</u><!--EndFragment--></body></html>",
                "<u>u</u>",
            ),
        ] {
            let cf = cf_html(html.as_bytes());
            assert_eq!(cf.last(), Some(&0));
            let head = String::from_utf8_lossy(&cf);
            let at = |key: &str| -> usize {
                head.lines()
                    .find_map(|l| l.strip_prefix(key))
                    .unwrap()
                    .trim_start_matches(':')
                    .parse()
                    .unwrap()
            };
            assert_eq!(
                &cf[at("StartFragment")..at("EndFragment")],
                fragment.as_bytes()
            );
            assert_eq!(at("EndHTML"), cf.len() - 1);
            let back = String::from_utf8(from_cf_html(&cf).unwrap()).unwrap();
            assert!(back.contains(fragment), "{back}");
            assert!(back.starts_with("<"));
        }
    }

    #[test]
    fn cf_html_without_document_offsets_gives_the_fragment() {
        let body = "<p>x</p>";
        let head = "Version:0.9\r\nStartHTML:-1\r\nEndHTML:-1\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n";
        let start = head.len();
        let data =
            head.replacen(
                "StartFragment:0000000000",
                &format!("StartFragment:{start:010}"),
                1,
            )
            .replacen(
                "EndFragment:0000000000",
                &format!("EndFragment:{:010}", start + body.len()),
                1,
            ) + body;
        assert_eq!(from_cf_html(data.as_bytes()).unwrap(), body.as_bytes());
        assert_eq!(from_cf_html(b"garbage"), None);
    }

    #[test]
    fn html_is_decoded_to_utf8() {
        let utf16: Vec<u8> = [0xff, 0xfe]
            .into_iter()
            .chain("<b>中</b>".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(utf8_html(&utf16), "<b>中</b>".as_bytes());
        assert_eq!(utf8_html(b"\xef\xbb\xbf<i>\0"), b"<i>");
        assert_eq!(utf8_html(b"<p>"), b"<p>");
    }
}
