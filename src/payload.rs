use crate::protocol::{FILE_LIMIT, IMAGE_LIMIT, Payload, ReadItem, TEXT_LIMIT};
use std::sync::{Arc, LazyLock};

pub const GNOME: &str = "x-special/gnome-copied-files";
pub const URI: &str = "text/uri-list";
pub const KDE: &str = "application/x-kde-cutselection";
/// Text types in order of preference.
pub const TEXT: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "TEXT",
    "STRING",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Image,
    Files,
}
#[derive(Clone, Debug)]
pub struct Format {
    pub kind: Kind,
    pub mime: String,
    pub cut: bool,
}
impl Format {
    pub fn select(types: &[String]) -> Option<Self> {
        if let Some(mime) = [GNOME, URI]
            .into_iter()
            .find(|mime| types.iter().any(|t| t == mime))
        {
            return Some(Self {
                kind: Kind::Files,
                mime: mime.to_owned(),
                cut: types.iter().any(|t| t == KDE),
            });
        }
        if let Some(mime) = types
            .iter()
            .find(|t| t.as_str() == "image/png")
            .or_else(|| types.iter().find(|t| t.starts_with("image/")))
        {
            return Some(Self {
                kind: Kind::Image,
                mime: mime.clone(),
                cut: false,
            });
        }
        TEXT.into_iter()
            .find(|mime| types.iter().any(|t| t == mime))
            .map(|mime| Self {
                kind: Kind::Text,
                mime: mime.to_owned(),
                cut: false,
            })
    }
    pub fn requests(&self) -> Vec<ReadItem> {
        let limit = match self.kind {
            Kind::Text => TEXT_LIMIT,
            Kind::Image => IMAGE_LIMIT,
            Kind::Files => FILE_LIMIT,
        };
        let mut items = Vec::with_capacity(if self.cut { 2 } else { 1 });
        items.push(ReadItem {
            mime: self.mime.clone(),
            limit,
        });
        if self.cut {
            items.push(ReadItem {
                mime: KDE.to_owned(),
                limit: 16,
            });
        }
        items
    }
    pub fn comparable(&self, other: &Self, other_types: &[String]) -> bool {
        self.kind == other.kind
            && (self.kind != Kind::Image || self.mime == other.mime)
            && (self.kind != Kind::Files
                || [GNOME, URI, KDE]
                    .iter()
                    .all(|mime| other_types.iter().any(|t| t == mime)))
    }
    pub fn normalize(&self, mut values: Vec<Arc<Vec<u8>>>) -> Option<Payload> {
        if values.len() != if self.cut { 2 } else { 1 } {
            return None;
        }
        let primary = values.remove(0);
        if primary.is_empty() {
            return None;
        }
        match self.kind {
            Kind::Image if primary.len() <= IMAGE_LIMIT => Some(Payload {
                formats: vec![(self.mime.clone(), primary)],
            }),
            Kind::Text if primary.len() <= TEXT_LIMIT => Some(Payload {
                formats: text_formats(primary)?,
            }),
            Kind::Files if primary.len() <= FILE_LIMIT => files(
                &self.mime,
                primary,
                values.first().map(|flag| flag.as_slice()),
            ),
            _ => None,
        }
    }
}

/// UTF-8 text under every text type, plus Latin-1 `STRING` when it fits.
pub fn text_formats(primary: Arc<Vec<u8>>) -> Option<Vec<(String, Arc<Vec<u8>>)>> {
    let text = std::str::from_utf8(&primary).ok()?;
    let latin1 = if text.is_ascii() {
        Some(Arc::clone(&primary))
    } else if text.chars().all(|c| u32::from(c) <= 255) {
        Some(Arc::new(text.chars().map(|c| c as u8).collect::<Vec<_>>()))
    } else {
        None
    };
    let mut formats = Vec::with_capacity(5);
    for mime in [
        "text/plain;charset=utf-8",
        "text/plain",
        "UTF8_STRING",
        "TEXT",
    ] {
        formats.push((mime.to_owned(), Arc::clone(&primary)));
    }
    if let Some(bytes) = latin1 {
        formats.push(("STRING".to_owned(), bytes));
    }
    Some(formats)
}

fn valid_uri(uri: &str) -> bool {
    let Some((scheme, _)) = uri.split_once(':') else {
        return false;
    };
    let mut scheme = scheme.bytes();
    if !scheme.next().is_some_and(|b| b.is_ascii_alphabetic())
        || !scheme.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    {
        return false;
    }
    let bytes = uri.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] <= 32 || bytes[i] == 127 {
            return false;
        }
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit()
            {
                return false;
            }
            i += 2;
        }
        i += 1;
    }
    true
}
/// A file list in every file type; `flag` is the KDE cut flag, if offered.
pub fn files(mime: &str, primary: Arc<Vec<u8>>, flag: Option<&[u8]>) -> Option<Payload> {
    let flag = match flag {
        None => None,
        Some(b"0") => Some(false),
        Some(b"1") => Some(true),
        _ => return None,
    };
    let text = std::str::from_utf8(&primary).ok()?;
    if text.ends_with('\r') {
        return None;
    }
    let mut lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    while lines.last() == Some(&"") {
        lines.pop();
    }
    let cut = if mime == GNOME {
        let action = match lines.first().copied() {
            Some("copy") => false,
            Some("cut") => true,
            _ => return None,
        };
        lines.remove(0);
        if flag.is_some_and(|flag| flag != action) {
            return None;
        }
        action
    } else if mime == URI {
        lines.retain(|line| !line.is_empty() && !line.starts_with('#'));
        flag.unwrap_or(false)
    } else {
        return None;
    };
    if lines.is_empty() || !lines.iter().all(|uri| valid_uri(uri)) {
        return None;
    }
    let action = if cut { "cut" } else { "copy" };
    let size = action.len() + lines.iter().map(|line| line.len() + 1).sum::<usize>();
    if size > FILE_LIMIT {
        return None;
    }
    let mut canonical = if mime == GNOME && primary.len() == size {
        None
    } else {
        let mut bytes = Vec::with_capacity(size);
        bytes.extend_from_slice(action.as_bytes());
        Some(bytes)
    };
    let uri_size = lines.iter().map(|line| line.len() + 2).sum();
    let mut uri_list = Vec::with_capacity(uri_size);
    for line in lines {
        if let Some(bytes) = &mut canonical {
            bytes.push(b'\n');
            bytes.extend_from_slice(line.as_bytes());
        }
        uri_list.extend_from_slice(line.as_bytes());
        uri_list.extend_from_slice(b"\r\n");
    }
    let canonical = canonical.map_or(primary, Arc::new);
    static FLAGS: LazyLock<[Arc<Vec<u8>>; 2]> =
        LazyLock::new(|| [Arc::new(vec![b'0']), Arc::new(vec![b'1'])]);
    Some(Payload {
        formats: vec![
            (GNOME.to_owned(), canonical),
            (URI.to_owned(), Arc::new(uri_list)),
            (KDE.to_owned(), Arc::clone(&FLAGS[usize::from(cut)])),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn convert(mime: &str, data: &[u8], flag: Option<&[u8]>) -> Option<Payload> {
        let format = Format {
            kind: Kind::Files,
            mime: mime.to_owned(),
            cut: flag.is_some(),
        };
        let mut values = vec![Arc::new(data.to_vec())];
        if let Some(flag) = flag {
            values.push(Arc::new(flag.to_vec()));
        }
        format.normalize(values)
    }
    #[test]
    fn encoded_multifile_cut_retains_order_and_action() {
        let input = b"cut\nfile:///tmp/%E4%B8%AD%E6%96%87%20name\nfile:///tmp/hash%23percent%25line%0A\nfile:///tmp/folder/";
        let value = convert(GNOME, input, None).unwrap();
        assert_eq!(value.data(GNOME).unwrap().as_ref(), input);
        assert_eq!(value.data(KDE).unwrap().as_ref(), b"1");
        assert_eq!(value.data(URI).unwrap().as_ref(), b"file:///tmp/%E4%B8%AD%E6%96%87%20name\r\nfile:///tmp/hash%23percent%25line%0A\r\nfile:///tmp/folder/\r\n");
    }
    #[test]
    fn kde_comments_and_crlf_preserve_cut_and_default_to_copy() {
        let input = b"# comment\r\nfile:///tmp/a\r\n\r\n";
        assert_eq!(
            convert(URI, input, Some(b"1"))
                .unwrap()
                .data(GNOME)
                .unwrap()
                .as_ref(),
            b"cut\nfile:///tmp/a"
        );
        assert_eq!(
            convert(URI, input, None)
                .unwrap()
                .data(GNOME)
                .unwrap()
                .as_ref(),
            b"copy\nfile:///tmp/a"
        );
        assert_eq!(
            convert(URI, input, Some(b"0"))
                .unwrap()
                .data(KDE)
                .unwrap()
                .as_ref(),
            b"0"
        );
    }
    #[test]
    fn invalid_or_conflicting_actions_do_not_downgrade_to_copy() {
        assert!(convert(GNOME, b"copy\nfile:///tmp/a", Some(b"1")).is_none());
        assert!(convert(URI, b"file:///tmp/a", Some(b"cut")).is_none());
        assert!(convert(GNOME, b"move\nfile:///tmp/a", None).is_none());
    }
    #[test]
    fn invalid_uris_and_encoding_are_rejected() {
        for input in [
            b"".as_slice(),
            b"/tmp/a",
            b"file:///tmp/a b",
            b"file:///tmp/a%ZZ",
            b"file:///tmp/a\0b",
            &[255],
        ] {
            assert!(convert(URI, input, None).is_none());
        }
        assert!(convert(GNOME, b"copy\nfile:///tmp/a\n\nfile:///tmp/b", None).is_none());
    }
    #[test]
    fn text_encodings_and_limits_are_consumer_visible() {
        let format = Format {
            kind: Kind::Text,
            mime: "UTF8_STRING".into(),
            cut: false,
        };
        let value = format
            .normalize(vec![Arc::new("caf\u{e9}".as_bytes().to_vec())])
            .unwrap();
        assert_eq!(value.data("STRING").unwrap().as_ref(), b"caf\xe9");
        assert_eq!(
            value.data("UTF8_STRING").unwrap().as_ref(),
            "café".as_bytes()
        );
        let unicode = format
            .normalize(vec![Arc::new("中文🦊".as_bytes().to_vec())])
            .unwrap();
        assert!(unicode.data("STRING").is_none());
        assert!(
            format
                .normalize(vec![Arc::new(vec![b'a'; TEXT_LIMIT + 1])])
                .is_none()
        );
        assert!(format.normalize(vec![Arc::new(vec![255])]).is_none());
    }
    #[test]
    fn image_wins_over_html_but_file_offer_wins_over_image() {
        let types = ["text/html", "text/plain", "image/jpeg", "image/png"].map(str::to_owned);
        assert_eq!(Format::select(&types).unwrap().mime, "image/png");
        let mut types = types.to_vec();
        types.push(URI.to_owned());
        assert_eq!(Format::select(&types).unwrap().kind, Kind::Files);
    }
}
