use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, mpsc};
use std::time::Duration;

pub const MARKER: &str = "application/x-nixos-clipboard-bridge";
pub const DEADLINE: Duration = Duration::from_secs(5);
pub const TEXT_LIMIT: usize = 512 * 1024;
pub const IMAGE_LIMIT: usize = 64 * 1024 * 1024;
pub const FILE_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Side {
    Wayland,
    X11,
    /// A passthrough guest (`guest`): no backend of its own, its copies
    /// arrive whole and are published on both displays.
    Guest,
}
impl Side {
    pub const fn index(self) -> usize {
        match self {
            Self::Wayland => 0,
            Self::X11 => 1,
            Self::Guest => 2,
        }
    }
    /// The other display.
    pub const fn other(self) -> Self {
        match self {
            Self::Wayland => Self::X11,
            Self::X11 => Self::Wayland,
            Self::Guest => panic!("the guest is not a display"),
        }
    }
}

#[derive(Default)]
pub struct Shared {
    pub epoch: AtomicU64,
    pub serials: [AtomicU64; 3],
}
impl Shared {
    pub fn selection(&self, side: Side, serial: u64, types: &[String]) -> u64 {
        self.serials[side.index()].store(serial, Ordering::SeqCst);
        if types.iter().any(|mime| mime == MARKER) {
            self.epoch.load(Ordering::SeqCst)
        } else {
            self.epoch.fetch_add(1, Ordering::SeqCst) + 1
        }
    }
    pub fn valid(&self, guard: Guard) -> bool {
        self.epoch.load(Ordering::SeqCst) == guard.epoch
            && self.serials[guard.source.index()].load(Ordering::SeqCst) == guard.serial
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Guard {
    pub epoch: u64,
    pub source: Side,
    pub serial: u64,
}
#[derive(Debug)]
pub struct ReadItem {
    pub mime: String,
    pub limit: usize,
}
#[derive(Debug, PartialEq, Eq)]
pub struct Payload {
    pub formats: Vec<(String, Arc<Vec<u8>>)>,
}
impl Payload {
    pub fn mimes(&self) -> impl Iterator<Item = &str> {
        self.formats
            .iter()
            .map(|(mime, _)| mime.as_str())
            .chain(std::iter::once(MARKER))
    }
    pub fn data(&self, mime: &str) -> Option<Arc<Vec<u8>>> {
        if mime == MARKER {
            static EMPTY: LazyLock<Arc<Vec<u8>>> = LazyLock::new(|| Arc::new(Vec::new()));
            return Some(Arc::clone(&EMPTY));
        }
        self.formats
            .iter()
            .find(|(name, _)| name == mime)
            .map(|(_, data)| Arc::clone(data))
    }
}
#[derive(Debug)]
pub enum Command {
    Read {
        id: u64,
        serial: u64,
        items: Vec<ReadItem>,
    },
    CancelReads,
    Publish {
        guard: Guard,
        payload: Arc<Payload>,
    },
    Stop,
}
#[derive(Debug)]
pub enum Event {
    Ready(Side),
    Offer {
        side: Side,
        serial: u64,
        epoch: u64,
        types: Vec<String>,
    },
    ReadComplete {
        side: Side,
        id: u64,
        serial: u64,
        data: Option<Vec<Arc<Vec<u8>>>>,
    },
    Failed {
        side: Side,
        error: String,
    },
    /// Where kvm-switch's router sends the input; None while it is not running.
    Switch(Option<crate::guest::Target>),
    /// A copy in the guest, already read and converted.
    GuestOffer(Payload),
    Shutdown,
}

pub struct Control {
    sender: mpsc::Sender<Command>,
    wake: UnixStream,
}
pub struct Commands {
    pub receiver: mpsc::Receiver<Command>,
    pub wake: UnixStream,
}
pub fn channel() -> io::Result<(Control, Commands)> {
    let (write, read) = UnixStream::pair()?;
    write.set_nonblocking(true)?;
    read.set_nonblocking(true)?;
    let (sender, receiver) = mpsc::channel();
    Ok((
        Control {
            sender,
            wake: write,
        },
        Commands {
            receiver,
            wake: read,
        },
    ))
}
impl Control {
    pub fn send(&self, command: Command) -> anyhow::Result<()> {
        self.sender.send(command)?;
        match (&self.wake).write(&[1]) {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}
impl Commands {
    pub fn drain_wake(&self) -> io::Result<()> {
        let mut bytes = [0; 256];
        loop {
            match (&self.wake).read(&mut bytes) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_new_genuine_copy_invalidates_an_older_publish() {
        let shared = Shared::default();
        let types = vec!["text/plain".to_owned()];
        let epoch = shared.selection(Side::Wayland, 1, &types);
        let guard = Guard {
            epoch,
            source: Side::Wayland,
            serial: 1,
        };
        assert!(shared.valid(guard));
        shared.selection(Side::X11, 1, &types);
        assert!(!shared.valid(guard));
    }
    #[test]
    fn delayed_peer_echo_does_not_cancel_a_genuine_copy() {
        let shared = Shared::default();
        let epoch = shared.selection(Side::Wayland, 7, &["image/png".into()]);
        let guard = Guard {
            epoch,
            source: Side::Wayland,
            serial: 7,
        };
        assert_eq!(shared.selection(Side::X11, 12, &[MARKER.into()]), epoch);
        assert!(shared.valid(guard));
    }
    #[test]
    fn source_owner_change_invalidates_even_before_target_discovery() {
        let shared = Shared::default();
        let epoch = shared.selection(Side::X11, 2, &["UTF8_STRING".into()]);
        let guard = Guard {
            epoch,
            source: Side::X11,
            serial: 2,
        };
        shared.serials[Side::X11.index()].store(3, Ordering::SeqCst);
        assert!(!shared.valid(guard));
    }
}
