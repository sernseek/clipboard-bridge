//! X11 clipboard backend.
//!
//! One hidden input-only window owns the CLIPBOARD selection when this backend
//! publishes. Incoming selections are inspected through private request windows
//! so that a cancelled transfer can never receive events belonging to a later
//! request. All transfers are deadline bounded and driven from a single event
//! loop: protocol events are drained before commands, then the X socket and the
//! command wake socket are polled until the nearest deadline.

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Sender, TryRecvError};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::Event as XEvent;
use x11rb::protocol::xfixes::{self, SelectionEvent, SelectionEventMask};
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ChangeWindowAttributesAux, CreateWindowAux, EventMask, PropMode, Window,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;

use crate::protocol::{
    Command, Commands, DEADLINE, Event, Guard, MARKER, Payload, ReadItem, Shared, Side,
};

const DATA_PROPERTY_NAME: &str = "CLIPBOARD_BRIDGE_DATA";
const TARGETS_PROPERTY_NAME: &str = "CLIPBOARD_BRIDGE_TARGETS";
const TIME_PROPERTY_NAME: &str = "CLIPBOARD_BRIDGE_TIME";

/// Payloads larger than this are served with INCR. The value stays well below
/// the default 256 KiB maximum request so a single chunk never depends on
/// BIG-REQUESTS.
const INCR_THRESHOLD: usize = 64 * 1024;
const INCR_CHUNK: usize = 64 * 1024;
const TARGET_ATOM_LIMIT: u32 = 4096;
const TARGET_NAME_LIMIT: usize = 4096;
const MIME_NAME_LIMIT: usize = 1024;

fn empty_data() -> Arc<Vec<u8>> {
    Arc::new(Vec::new())
}

fn next_deadline() -> Instant {
    Instant::now() + DEADLINE
}

fn reply_or_connection<T>(result: std::result::Result<T, ReplyError>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        // A per-client X11 error (BadWindow, BadAtom, ...) only fails the
        // request it belongs to. It must never take the backend down.
        Err(ReplyError::X11Error(_)) => Ok(None),
        Err(ReplyError::ConnectionError(error)) => Err(anyhow!("X11 connection error: {error}")),
    }
}

fn id_error(error: x11rb::errors::ReplyOrIdError) -> anyhow::Error {
    anyhow!("X11 id allocation failed: {error}")
}

fn intern_name(
    conn: &RustConnection,
    cache: &mut HashMap<String, Atom>,
    name: &str,
) -> Result<Atom> {
    if let Some(atom) = cache.get(name) {
        return Ok(*atom);
    }
    let cookie = xproto::intern_atom(conn, false, name.as_bytes())
        .with_context(|| format!("intern atom {name:?}"))?;
    let reply = cookie
        .reply()
        .with_context(|| format!("intern atom {name:?}"))?;
    cache.insert(name.to_string(), reply.atom);
    Ok(reply.atom)
}

fn words_bytes(values: &[u32], msb: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len().saturating_mul(4));
    for &value in values {
        let bytes = if msb {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        };
        out.extend_from_slice(&bytes);
    }
    out
}

/// X11 timestamps are 32-bit milliseconds and wrap; compare them as signed
/// differences the same way the server does.
fn time_before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

fn read_u32(bytes: &[u8], msb: bool) -> Option<u32> {
    let head = bytes.get(..4)?;
    let array = [head[0], head[1], head[2], head[3]];
    Some(if msb {
        u32::from_be_bytes(array)
    } else {
        u32::from_le_bytes(array)
    })
}

/// The advisory size stored in an INCR header.
///
/// Stock xclip stores a format32 header with **zero elements**, so an empty
/// value and a stored zero both mean "size unknown".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IncrHint {
    Unknown,
    Size(usize),
}

/// Parse the INCR property header. `None` means a malformed (truncated)
/// format32 value. The hint is advisory only: the actual payload is bounded
/// separately by the configured limit and the transfer deadline.
fn parse_incr_announcement(value: &[u8], msb: bool) -> Option<IncrHint> {
    if value.is_empty() {
        return Some(IncrHint::Unknown);
    }
    let size = read_u32(value, msb)? as usize;
    Some(if size == 0 {
        IncrHint::Unknown
    } else {
        IncrHint::Size(size)
    })
}

/// Append an INCR chunk while enforcing the payload limit. Returns false and
/// leaves the buffer unchanged when the chunk would exceed the limit.
fn append_incr_chunk(buffer: &mut Vec<u8>, chunk: &[u8], limit: usize) -> bool {
    let next = buffer.len().saturating_add(chunk.len());
    if next > limit {
        return false;
    }
    buffer.extend_from_slice(chunk);
    true
}

/// Latin-1 to UTF-8 conversion for a property whose type is STRING.
fn latin1_to_utf8(data: &[u8], limit: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len().saturating_mul(2).min(limit));
    for &byte in data {
        if byte < 0x80 {
            out.push(byte);
        } else {
            out.push(0xC0 | (byte >> 6));
            out.push(0x80 | (byte & 0x3F));
        }
        if out.len() > limit {
            return None;
        }
    }
    Some(out)
}

fn text_data(payload: &Payload) -> Option<Arc<Vec<u8>>> {
    const PREFERRED: [&str; 6] = [
        "text/plain;charset=utf-8",
        "text/plain;charset=UTF-8",
        "text/plain",
        "UTF8_STRING",
        "TEXT",
        "STRING",
    ];
    for name in PREFERRED {
        if let Some((_, data)) = payload.formats.iter().find(|(mime, _)| mime == name) {
            return Some(Arc::clone(data));
        }
    }
    payload
        .formats
        .iter()
        .find(|(mime, _)| mime.starts_with("text/"))
        .map(|(_, data)| Arc::clone(data))
}

fn long_length(limit: usize) -> u32 {
    let units = limit.saturating_add(3) / 4;
    units.max(1).min(u32::MAX as usize) as u32
}

fn duration_to_millis(deadline: Instant) -> i32 {
    let now = Instant::now();
    if deadline <= now {
        return 0;
    }
    let nanos = (deadline - now).as_nanos();
    let millis = nanos.div_ceil(1_000_000);
    millis.min(i32::MAX as u128) as i32
}

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    timestamp: Atom,
    incr: Atom,
    atom: Atom,
    integer: Atom,
    string: Atom,
    utf8_string: Atom,
    text: Atom,
    marker: Atom,
    data_property: Atom,
    targets_property: Atom,
    time_property: Atom,
}

#[derive(Clone)]
enum TargetKind {
    Targets,
    Timestamp,
    Utf8(Arc<Vec<u8>>),
    Text(Arc<Vec<u8>>),
    String(Arc<Vec<u8>>),
    Mime(Arc<Vec<u8>>),
    Marker,
}

#[derive(Clone)]
struct OwnTarget {
    atom: Atom,
    kind: TargetKind,
}

struct Owned {
    payload: Arc<Payload>,
    targets: Vec<OwnTarget>,
    time: u32,
}

struct PendingPublish {
    guard: Guard,
    payload: Arc<Payload>,
    targets: Vec<OwnTarget>,
    mimes: Vec<String>,
    deadline: Instant,
}

struct TargetsPending {
    window: Window,
    owner: Window,
    serial: u64,
    deadline: Instant,
}

enum IncomingState {
    AwaitingNotify,
    Incr { data_type: Atom, buffer: Vec<u8> },
    Complete(Arc<Vec<u8>>),
}

struct IncomingItem {
    window: Window,
    limit: usize,
    state: IncomingState,
}

struct ReadJob {
    id: u64,
    serial: u64,
    deadline: Instant,
    items: Vec<IncomingItem>,
}

struct OutTransfer {
    requestor: Window,
    property: Atom,
    type_: Atom,
    data: Arc<Vec<u8>>,
    offset: usize,
    deadline: Instant,
}

struct Backend {
    conn: RustConnection,
    root: Window,
    window: Window,
    atoms: Atoms,
    atom_cache: HashMap<String, Atom>,
    msb: bool,

    commands: Commands,
    events: Sender<Event>,
    shared: Arc<Shared>,

    /// Backend-local monotonic serial. Stored into `Shared` for every owner
    /// change so stale source guards are invalidated immediately.
    next_serial: u64,
    latest_serial: u64,

    /// Last owner observed on the X11 side (0 means no owner).
    owner: Window,
    /// Set once a real selection (external or ours) has existed. Only then may
    /// an explicit clear advance the global epoch.
    had_selection: bool,

    own: Option<Owned>,
    targets_pending: Option<TargetsPending>,

    reads: Vec<ReadJob>,
    publish_wait: Option<PendingPublish>,
    deferred_publish: Option<PendingPublish>,
    transfers: Vec<OutTransfer>,
}

impl Backend {
    fn init(commands: Commands, events: Sender<Event>, shared: Arc<Shared>) -> Result<Self> {
        let (conn, screen) = RustConnection::connect(None).context("connect to the X11 display")?;
        let root = conn
            .setup()
            .roots
            .get(screen)
            .map(|screen| screen.root)
            .ok_or_else(|| anyhow!("X11 screen {screen} is not present"))?;
        // x11rb uses native connection byte order; server pixel order is unrelated.
        let msb = cfg!(target_endian = "big");

        // Resolving the extension here also teaches x11rb how to parse XFixes
        // events before the first poll.
        xfixes::query_version(&conn, 5, 0)
            .context("the X server lacks the XFixes extension")?
            .reply()
            .context("query XFixes version")?;

        let mut atom_cache = HashMap::new();
        let clipboard = intern_name(&conn, &mut atom_cache, "CLIPBOARD")?;
        let targets = intern_name(&conn, &mut atom_cache, "TARGETS")?;
        let timestamp = intern_name(&conn, &mut atom_cache, "TIMESTAMP")?;
        let incr = intern_name(&conn, &mut atom_cache, "INCR")?;
        let atom = intern_name(&conn, &mut atom_cache, "ATOM")?;
        let integer = intern_name(&conn, &mut atom_cache, "INTEGER")?;
        let string = intern_name(&conn, &mut atom_cache, "STRING")?;
        let utf8_string = intern_name(&conn, &mut atom_cache, "UTF8_STRING")?;
        let text = intern_name(&conn, &mut atom_cache, "TEXT")?;
        let marker = intern_name(&conn, &mut atom_cache, MARKER)?;
        let data_property = intern_name(&conn, &mut atom_cache, DATA_PROPERTY_NAME)?;
        let targets_property = intern_name(&conn, &mut atom_cache, TARGETS_PROPERTY_NAME)?;
        let time_property = intern_name(&conn, &mut atom_cache, TIME_PROPERTY_NAME)?;

        let window = conn.generate_id().map_err(id_error)?;
        xproto::create_window(
            &conn,
            0,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .context("create the clipboard owner window")?;

        xfixes::select_selection_input(
            &conn,
            window,
            clipboard,
            SelectionEventMask::SET_SELECTION_OWNER
                | SelectionEventMask::SELECTION_WINDOW_DESTROY
                | SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )
        .context("select XFixes clipboard notifications")?;
        conn.flush().context("flush X11 setup requests")?;

        let first_owner_cookie =
            xproto::get_selection_owner(&conn, clipboard).context("query the CLIPBOARD owner")?;
        let first_owner = first_owner_cookie
            .reply()
            .context("query the CLIPBOARD owner")?
            .owner;

        let mut backend = Self {
            conn,
            root,
            window,
            atoms: Atoms {
                clipboard,
                targets,
                timestamp,
                incr,
                atom,
                integer,
                string,
                utf8_string,
                text,
                marker,
                data_property,
                targets_property,
                time_property,
            },
            atom_cache,
            msb,
            commands,
            events,
            shared,
            next_serial: 0,
            latest_serial: 0,
            owner: 0,
            had_selection: false,
            own: None,
            targets_pending: None,
            reads: Vec::new(),
            publish_wait: None,
            deferred_publish: None,
            transfers: Vec::new(),
        };

        backend.emit(Event::Ready(Side::X11));

        // Startup with no owner emits no Offer at all: an empty non-advancing
        // Offer is indistinguishable from a real clear in Main and could race
        // a genuine peer copy. An existing owner is offered once resolved.
        if first_owner != 0 {
            backend.owner = first_owner;
            backend.had_selection = true;
            let serial = backend.advance_serial();
            backend.start_targets(first_owner, serial)?;
        }

        Ok(backend)
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    fn advance_serial(&mut self) -> u64 {
        self.next_serial = self.next_serial.saturating_add(1);
        self.latest_serial = self.next_serial;
        self.shared.serials[Side::X11.index()].store(self.latest_serial, Ordering::SeqCst);
        self.latest_serial
    }

    fn new_window(&self) -> Result<Window> {
        let window = self.conn.generate_id().map_err(id_error)?;
        xproto::create_window(
            &self.conn,
            0,
            window,
            self.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .context("create a clipboard request window")?;
        Ok(window)
    }

    fn destroy_window(&self, window: Window) {
        let _ = xproto::destroy_window(&self.conn, window);
    }

    fn delete_property(&self, window: Window, property: Atom) {
        let _ = xproto::delete_property(&self.conn, window, property);
    }

    fn intern(&mut self, name: &str) -> Result<Atom> {
        intern_name(&self.conn, &mut self.atom_cache, name)
    }

    fn current_owner(&self) -> Result<Window> {
        let cookie = xproto::get_selection_owner(&self.conn, self.atoms.clipboard)
            .context("query the CLIPBOARD owner")?;
        match reply_or_connection(cookie.reply())? {
            Some(reply) => Ok(reply.owner),
            None => Ok(0),
        }
    }

    fn event_loop(&mut self) -> Result<()> {
        loop {
            self.drain_events()?;
            self.expire_deadlines()?;

            self.commands
                .drain_wake()
                .context("drain the command wake socket")?;
            loop {
                match self.commands.receiver.try_recv() {
                    Ok(command) => {
                        if self.handle_command(command)? {
                            self.shutdown();
                            return Ok(());
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.shutdown();
                        return Ok(());
                    }
                }
            }

            // Command handlers perform short synchronous X round-trips (atom
            // interning). Events read while waiting for those replies are in
            // x11rb's internal queue, so drain again before sleeping in poll.
            self.drain_events()?;
            self.expire_deadlines()?;

            self.conn.flush().context("flush X11 requests")?;

            let x_fd = self.conn.stream().as_raw_fd();
            let wake_fd = self.commands.wake.as_raw_fd();
            let timeout = self
                .nearest_deadline()
                .map(duration_to_millis)
                .unwrap_or(-1);
            let mut descriptors = [
                libc::pollfd {
                    fd: x_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: wake_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: both descriptors are owned by `self` and remain valid for
            // the duration of the call; the array is correctly sized.
            let result = unsafe {
                libc::poll(
                    descriptors.as_mut_ptr(),
                    descriptors.len() as libc::nfds_t,
                    timeout,
                )
            };
            if result < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(error).context("poll X11 and command sockets");
            }
        }
    }

    fn nearest_deadline(&self) -> Option<Instant> {
        let mut nearest = None;
        let mut consider = |deadline: Instant| {
            nearest = Some(match nearest {
                Some(current) if current <= deadline => current,
                _ => deadline,
            });
        };
        if let Some(pending) = &self.targets_pending {
            consider(pending.deadline);
        }
        for read in &self.reads {
            consider(read.deadline);
        }
        if let Some(pending) = &self.publish_wait {
            consider(pending.deadline);
        }
        for transfer in &self.transfers {
            consider(transfer.deadline);
        }
        nearest
    }

    fn drain_events(&mut self) -> Result<()> {
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(event)) => self.handle_event(event)?,
                Ok(None) => return Ok(()),
                Err(error) => return Err(anyhow!("read X11 events: {error}")),
            }
        }
    }

    fn handle_event(&mut self, event: XEvent) -> Result<()> {
        match event {
            XEvent::XfixesSelectionNotify(event) => {
                if event.selection != self.atoms.clipboard {
                    return Ok(());
                }
                match event.subtype {
                    SelectionEvent::SET_SELECTION_OWNER => {
                        if event.owner == self.window {
                            // Our own acquisition is committed synchronously in
                            // `finish_publish`; nothing to reconcile here.
                            return Ok(());
                        }
                        self.note_external_owner(event.owner)?;
                    }
                    SelectionEvent::SELECTION_WINDOW_DESTROY
                    | SelectionEvent::SELECTION_CLIENT_CLOSE
                        if self.owner != self.window =>
                    {
                        // The owner field of these events is not a reliable
                        // statement about the surviving owner; query it.
                        let owner = self.current_owner()?;
                        if owner != self.owner && owner != self.window {
                            self.note_external_owner(owner)?;
                        }
                    }
                    _ => {}
                }
            }
            XEvent::SelectionClear(event) => {
                if event.selection != self.atoms.clipboard || event.owner != self.window {
                    return Ok(());
                }
                let Some(own) = self.own.as_ref() else {
                    return Ok(());
                };
                if time_before(event.time, own.time) {
                    // A clear that predates our latest acquisition is stale.
                    return Ok(());
                }
                // The XFixes owner notification that follows assigns the new
                // serial. Until then serve nothing and fail any in-flight read.
                self.own = None;
                self.owner = 0;
                self.cancel_targets();
                self.fail_all_reads();
                if let Some(pending) = self.publish_wait.take() {
                    self.deferred_publish = Some(pending);
                }
            }
            XEvent::SelectionRequest(event) => self.handle_selection_request(event)?,
            XEvent::SelectionNotify(event) => self.handle_selection_notify(event)?,
            XEvent::PropertyNotify(event) => self.handle_property_notify(event)?,
            XEvent::DestroyNotify(event) => {
                if event.window == self.window {
                    return Err(anyhow!("the clipboard owner window was destroyed"));
                }
                self.transfers
                    .retain(|transfer| transfer.requestor != event.window);
            }
            XEvent::Error(_) | XEvent::Unknown(_) => {
                // Dropped void cookies surface per-client errors here. A
                // BadWindow from an early-closing requestor is not fatal.
            }
            _ => {}
        }
        Ok(())
    }

    fn note_external_owner(&mut self, owner: Window) -> Result<()> {
        if owner == self.window {
            return Ok(());
        }

        let clear_without_selection = owner == 0 && !self.had_selection;

        self.owner = owner;
        self.own = None;
        self.cancel_targets();
        self.fail_all_reads();
        if let Some(pending) = self.publish_wait.take() {
            self.deferred_publish = Some(pending);
        }

        if clear_without_selection {
            // Nothing real has existed yet. Do not touch the shared serial or
            // epoch and do not emit an empty Offer that Main could confuse
            // with a genuine selection event.
            self.maybe_start_deferred_publish()?;
            return Ok(());
        }

        // Update the shared serial before resolution so stale X11 guards are
        // rejected immediately, including reads queued behind the owner change.
        let serial = self.advance_serial();
        if owner == 0 {
            // An explicit clear after a real/owned selection invalidates.
            let epoch = self.shared.selection(Side::X11, serial, &[]);
            self.emit(Event::Offer {
                side: Side::X11,
                serial,
                epoch,
                types: Vec::new(),
            });
            self.maybe_start_deferred_publish()?;
        } else {
            self.had_selection = true;
            self.start_targets(owner, serial)?;
        }
        Ok(())
    }

    fn start_targets(&mut self, owner: Window, serial: u64) -> Result<()> {
        let window = self.new_window()?;
        xproto::convert_selection(
            &self.conn,
            window,
            self.atoms.clipboard,
            self.atoms.targets,
            self.atoms.targets_property,
            0u32,
        )
        .context("request TARGETS from the clipboard owner")?;
        self.targets_pending = Some(TargetsPending {
            window,
            owner,
            serial,
            deadline: next_deadline(),
        });
        Ok(())
    }

    fn cancel_targets(&mut self) {
        if let Some(pending) = self.targets_pending.take() {
            self.destroy_window(pending.window);
        }
    }

    fn handle_selection_notify(&mut self, event: xproto::SelectionNotifyEvent) -> Result<()> {
        if event.selection != self.atoms.clipboard {
            return Ok(());
        }
        if let Some(pending) = self.targets_pending.take() {
            // The request window is private to this TARGETS conversion.
            if pending.window == event.requestor {
                return self.finish_targets(pending, event.property);
            }
            self.targets_pending = Some(pending);
        }

        let mut found = None;
        for (job_index, job) in self.reads.iter().enumerate() {
            for (item_index, item) in job.items.iter().enumerate() {
                if item.window == event.requestor
                    && matches!(&item.state, IncomingState::AwaitingNotify)
                {
                    found = Some((job_index, item_index));
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        let Some((job_index, item_index)) = found else {
            return Ok(());
        };

        if event.property == 0 {
            self.fail_job(job_index);
            return Ok(());
        }
        self.read_item_notify(job_index, item_index, event.property)
    }

    fn finish_targets(&mut self, pending: TargetsPending, property: Atom) -> Result<()> {
        let atoms = if property == 0 {
            None
        } else {
            self.fetch_target_atoms(pending.window, property)?
        };
        self.destroy_window(pending.window);

        // The owner may have changed while the TARGETS transfer was running.
        let current = self.current_owner()?;
        if current == self.window {
            return Ok(());
        }
        if current != pending.owner {
            self.note_external_owner(current)?;
            return Ok(());
        }
        if pending.serial != self.latest_serial {
            return Ok(());
        }

        let mut types = Vec::new();
        if let Some(atoms) = atoms {
            for atom in atoms {
                if atom == 0 || types.len() >= TARGET_NAME_LIMIT {
                    continue;
                }
                let cookie = match xproto::get_atom_name(&self.conn, atom) {
                    Ok(cookie) => cookie,
                    Err(error) => return Err(anyhow!("query target atom name: {error}")),
                };
                let Some(reply) = reply_or_connection(cookie.reply())? else {
                    continue;
                };
                if reply.name.len() > TARGET_NAME_LIMIT {
                    continue;
                }
                let Ok(name) = String::from_utf8(reply.name) else {
                    continue;
                };
                if name.is_empty() || name.len() > MIME_NAME_LIMIT {
                    continue;
                }
                if !types.iter().any(|existing| existing == &name) {
                    types.push(name);
                }
            }
        }

        self.had_selection = true;
        let epoch = self.shared.selection(Side::X11, pending.serial, &types);
        self.emit(Event::Offer {
            side: Side::X11,
            serial: pending.serial,
            epoch,
            types,
        });
        self.maybe_start_deferred_publish()
    }

    fn fetch_target_atoms(&self, window: Window, property: Atom) -> Result<Option<Vec<Atom>>> {
        let cookie = xproto::get_property(
            &self.conn,
            true,
            window,
            property,
            AtomEnum::ANY,
            0,
            TARGET_ATOM_LIMIT,
        )
        .context("read the TARGETS property")?;
        let Some(reply) = reply_or_connection(cookie.reply())? else {
            return Ok(None);
        };
        if reply.type_ != self.atoms.atom || reply.format != 32 || reply.bytes_after != 0 {
            return Ok(None);
        }
        if reply.value.len() % 4 != 0 {
            return Ok(None);
        }
        let mut atoms = Vec::with_capacity(reply.value.len() / 4);
        for chunk in reply.value.as_chunks::<4>().0 {
            let Some(atom) = read_u32(chunk, self.msb) else {
                return Ok(None);
            };
            atoms.push(atom);
            if atoms.len() >= TARGET_ATOM_LIMIT as usize {
                break;
            }
        }
        Ok(Some(atoms))
    }

    fn maybe_start_deferred_publish(&mut self) -> Result<()> {
        if self.targets_pending.is_some() || self.publish_wait.is_some() {
            return Ok(());
        }
        if let Some(pending) = self.deferred_publish.take()
            && self.shared.valid(pending.guard)
        {
            self.begin_publish(pending)?;
        }
        Ok(())
    }

    fn begin_publish(&mut self, mut pending: PendingPublish) -> Result<()> {
        if !self.shared.valid(pending.guard) {
            return Ok(());
        }
        if self.targets_pending.is_some() {
            self.deferred_publish = Some(pending);
            return Ok(());
        }
        pending.deadline = next_deadline();
        if self.publish_wait.is_some() {
            // The pending time request will still deliver a fresh timestamp.
            self.publish_wait = Some(pending);
            return Ok(());
        }
        self.publish_wait = Some(pending);

        // A fresh server timestamp for SetSelectionOwner: a user copy that
        // happens after this point carries a newer timestamp and wins.
        self.delete_property(self.window, self.atoms.time_property);
        xproto::change_property(
            &self.conn,
            PropMode::REPLACE,
            self.window,
            self.atoms.time_property,
            AtomEnum::INTEGER,
            8,
            0,
            &[],
        )
        .context("request a fresh X11 timestamp")?;
        Ok(())
    }

    fn finish_publish(&mut self, pending: PendingPublish, time: u32) -> Result<()> {
        if self.targets_pending.is_some() {
            self.deferred_publish = Some(pending);
            return Ok(());
        }
        if !self.shared.valid(pending.guard) {
            return Ok(());
        }

        xproto::set_selection_owner(&self.conn, self.window, self.atoms.clipboard, time)
            .context("take the CLIPBOARD selection")?;
        // Ordered after SetSelectionOwner; the reply tells whether a newer
        // selection timestamp won.
        let owner = self.current_owner()?;
        if owner == self.window {
            self.commit_own(pending, time);
        } else if owner != self.owner || owner == 0 {
            self.note_external_owner(owner)?;
        }
        Ok(())
    }

    fn commit_own(&mut self, pending: PendingPublish, time: u32) {
        self.cancel_targets();
        self.fail_all_reads();
        self.owner = self.window;
        self.had_selection = true;
        let serial = self.advance_serial();
        self.own = Some(Owned {
            payload: pending.payload,
            targets: pending.targets,
            time,
        });
        // Own offers carry the marker, so `Shared::selection` leaves the epoch
        // untouched.
        let epoch = self.shared.selection(Side::X11, serial, &pending.mimes);
        self.emit(Event::Offer {
            side: Side::X11,
            serial,
            epoch,
            types: pending.mimes,
        });
    }

    fn build_publish(&mut self, guard: Guard, payload: Arc<Payload>) -> Result<PendingPublish> {
        let mut targets = Vec::new();
        let push = |atom: Atom, kind: TargetKind, targets: &mut Vec<OwnTarget>| {
            if !targets.iter().any(|existing| existing.atom == atom) {
                targets.push(OwnTarget { atom, kind });
            }
        };

        let text = text_data(&payload);

        push(self.atoms.timestamp, TargetKind::Timestamp, &mut targets);
        push(self.atoms.targets, TargetKind::Targets, &mut targets);
        if let Some(data) = &text {
            push(
                self.atoms.utf8_string,
                TargetKind::Utf8(Arc::clone(data)),
                &mut targets,
            );
            push(
                self.atoms.text,
                TargetKind::Text(Arc::clone(data)),
                &mut targets,
            );
            if let Some(latin1) = payload.data("STRING") {
                push(self.atoms.string, TargetKind::String(latin1), &mut targets);
            }
        }

        for (mime, data) in &payload.formats {
            if mime.is_empty() || mime == MARKER {
                continue;
            }
            let atom = self.intern(mime)?;
            push(atom, TargetKind::Mime(Arc::clone(data)), &mut targets);
        }

        push(self.atoms.marker, TargetKind::Marker, &mut targets);

        let mimes = payload.mimes().map(str::to_string).collect();
        Ok(PendingPublish {
            guard,
            payload,
            targets,
            mimes,
            deadline: next_deadline(),
        })
    }

    fn handle_property_notify(&mut self, event: xproto::PropertyNotifyEvent) -> Result<()> {
        if event.window == self.window
            && event.atom == self.atoms.time_property
            && event.state == xproto::Property::NEW_VALUE
        {
            if let Some(pending) = self.publish_wait.take() {
                return self.finish_publish(pending, event.time);
            }
            return Ok(());
        }

        if event.state == xproto::Property::NEW_VALUE && event.atom == self.atoms.data_property {
            let mut found = None;
            for (job_index, job) in self.reads.iter().enumerate() {
                for (item_index, item) in job.items.iter().enumerate() {
                    if item.window == event.window
                        && matches!(&item.state, IncomingState::Incr { .. })
                    {
                        found = Some((job_index, item_index));
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }
            if let Some((job_index, item_index)) = found {
                return self.read_incr_chunk(job_index, item_index);
            }
            return Ok(());
        }

        if event.state == xproto::Property::DELETE
            && let Some(index) = self.transfers.iter().position(|transfer| {
                transfer.requestor == event.window && transfer.property == event.atom
            })
        {
            return self.advance_transfer(index);
        }
        Ok(())
    }

    fn read_item_notify(
        &mut self,
        job_index: usize,
        item_index: usize,
        property: Atom,
    ) -> Result<()> {
        let (window, limit) = {
            let item = &self.reads[job_index].items[item_index];
            (item.window, item.limit)
        };

        let cookie = xproto::get_property(
            &self.conn,
            true,
            window,
            property,
            AtomEnum::ANY,
            0,
            long_length(limit),
        )
        .context("read clipboard property")?;
        let Some(reply) = reply_or_connection(cookie.reply())? else {
            self.fail_job(job_index);
            return Ok(());
        };
        if reply.type_ == 0 || reply.bytes_after != 0 {
            self.fail_job(job_index);
            return Ok(());
        }

        if reply.type_ == self.atoms.incr {
            if reply.format != 32 {
                self.fail_job(job_index);
                return Ok(());
            }
            let Some(hint) = parse_incr_announcement(&reply.value, self.msb) else {
                self.fail_job(job_index);
                return Ok(());
            };
            // A lower-bound hint larger than the limit guarantees truncation,
            // so failing early is safe.
            if matches!(hint, IncrHint::Size(size) if size > limit) {
                self.fail_job(job_index);
                return Ok(());
            }
            // `get_property(delete = true)` performed the INCR handshake.
            self.reads[job_index].items[item_index].state = IncomingState::Incr {
                data_type: 0,
                buffer: Vec::new(),
            };
            return Ok(());
        }

        if reply.format != 8 || reply.value.len() > limit {
            self.fail_job(job_index);
            return Ok(());
        }
        let bytes: Arc<Vec<u8>> = if reply.type_ == self.atoms.string {
            match latin1_to_utf8(&reply.value, limit) {
                Some(converted) => Arc::new(converted),
                None => {
                    self.fail_job(job_index);
                    return Ok(());
                }
            }
        } else {
            Arc::new(reply.value)
        };
        self.complete_item(job_index, item_index, bytes);
        Ok(())
    }

    fn read_incr_chunk(&mut self, job_index: usize, item_index: usize) -> Result<()> {
        let (window, limit, current_type) = {
            let item = &self.reads[job_index].items[item_index];
            match &item.state {
                IncomingState::Incr { data_type, .. } => (item.window, item.limit, *data_type),
                _ => return Ok(()),
            }
        };

        let cookie = xproto::get_property(
            &self.conn,
            true,
            window,
            self.atoms.data_property,
            AtomEnum::ANY,
            0,
            long_length(limit),
        )
        .context("read INCR chunk")?;
        let Some(reply) = reply_or_connection(cookie.reply())? else {
            self.fail_job(job_index);
            return Ok(());
        };
        if reply.type_ == 0 || reply.format != 8 || reply.bytes_after != 0 {
            self.fail_job(job_index);
            return Ok(());
        }

        let reply_type = reply.type_;
        if current_type != 0 && reply_type != current_type {
            self.fail_job(job_index);
            return Ok(());
        }
        if reply_type == self.atoms.incr {
            self.fail_job(job_index);
            return Ok(());
        }

        if reply.value.is_empty() {
            let buffer = {
                let item = &mut self.reads[job_index].items[item_index];
                match &mut item.state {
                    IncomingState::Incr { buffer, .. } => std::mem::take(buffer),
                    _ => return Ok(()),
                }
            };
            let bytes: Arc<Vec<u8>> = if reply_type == self.atoms.string {
                match latin1_to_utf8(&buffer, limit) {
                    Some(converted) => Arc::new(converted),
                    None => {
                        self.fail_job(job_index);
                        return Ok(());
                    }
                }
            } else {
                Arc::new(buffer)
            };
            self.complete_item(job_index, item_index, bytes);
            return Ok(());
        }

        // The advertised size is only a lower-bound hint; the real payload is
        // bounded by the item limit (and the transfer deadline).
        let appended = {
            let item = &mut self.reads[job_index].items[item_index];
            match &mut item.state {
                IncomingState::Incr { data_type, buffer } => {
                    if append_incr_chunk(buffer, &reply.value, limit) {
                        *data_type = reply_type;
                        true
                    } else {
                        false
                    }
                }
                _ => true,
            }
        };
        if !appended {
            self.fail_job(job_index);
        }
        Ok(())
    }

    fn complete_item(&mut self, job_index: usize, item_index: usize, data: Arc<Vec<u8>>) {
        let window = {
            let item = &mut self.reads[job_index].items[item_index];
            item.state = IncomingState::Complete(data);
            item.window
        };
        self.destroy_window(window);

        let all_done = self.reads[job_index]
            .items
            .iter()
            .all(|item| matches!(&item.state, IncomingState::Complete(_)));
        if !all_done {
            return;
        }

        let job = self.reads.remove(job_index);
        let mut data = Vec::with_capacity(job.items.len());
        let mut complete = true;
        for item in job.items {
            match item.state {
                IncomingState::Complete(buffer) => data.push(buffer),
                _ => {
                    complete = false;
                    break;
                }
            }
        }
        self.emit(Event::ReadComplete {
            side: Side::X11,
            id: job.id,
            serial: job.serial,
            data: if complete { Some(data) } else { None },
        });
    }

    fn fail_job(&mut self, job_index: usize) {
        let job = self.reads.remove(job_index);
        for item in &job.items {
            self.destroy_window(item.window);
        }
        self.emit(Event::ReadComplete {
            side: Side::X11,
            id: job.id,
            serial: job.serial,
            data: None,
        });
    }

    fn fail_all_reads(&mut self) {
        let jobs = std::mem::take(&mut self.reads);
        for job in jobs {
            for item in &job.items {
                self.destroy_window(item.window);
            }
            self.emit(Event::ReadComplete {
                side: Side::X11,
                id: job.id,
                serial: job.serial,
                data: None,
            });
        }
    }

    fn handle_selection_request(&mut self, request: xproto::SelectionRequestEvent) -> Result<()> {
        if request.selection != self.atoms.clipboard || request.owner != self.window {
            return Ok(());
        }

        let mut property = request.property;
        if property == 0 {
            // ICCCM fallback for obsolete requestors.
            property = request.target;
        }
        if property == 0 {
            return self.send_selection_notify(&request, 0);
        }

        let (kind, time, target_atoms) = {
            let Some(own) = self.own.as_ref() else {
                return self.send_selection_notify(&request, 0);
            };
            let kind = own
                .targets
                .iter()
                .find(|target| target.atom == request.target)
                .map(|target| target.kind.clone());
            let atoms = if matches!(&kind, Some(TargetKind::Targets)) {
                own.targets.iter().map(|target| target.atom).collect()
            } else {
                Vec::new()
            };
            (kind, own.time, atoms)
        };

        let Some(kind) = kind else {
            return self.send_selection_notify(&request, 0);
        };

        match kind {
            TargetKind::Targets => {
                self.serve_words(&request, property, self.atoms.atom, &target_atoms)?;
            }
            TargetKind::Timestamp => {
                self.serve_words(&request, property, self.atoms.integer, &[time])?;
            }
            TargetKind::Utf8(data) | TargetKind::Text(data) => {
                self.serve_bytes(&request, property, self.atoms.utf8_string, data)?;
            }
            TargetKind::String(data) => {
                self.serve_bytes(&request, property, self.atoms.string, data)?;
            }
            TargetKind::Mime(data) => {
                self.serve_bytes(&request, property, request.target, data)?;
            }
            TargetKind::Marker => {
                self.serve_bytes(&request, property, self.atoms.marker, empty_data())?;
            }
        }
        Ok(())
    }

    fn serve_words(
        &self,
        request: &xproto::SelectionRequestEvent,
        property: Atom,
        type_: Atom,
        values: &[u32],
    ) -> Result<()> {
        let bytes = words_bytes(values, self.msb);
        xproto::change_property(
            &self.conn,
            PropMode::REPLACE,
            request.requestor,
            property,
            type_,
            32,
            values.len() as u32,
            &bytes,
        )
        .context("write selection property")?;
        self.send_selection_notify(request, property)
    }

    fn serve_bytes(
        &mut self,
        request: &xproto::SelectionRequestEvent,
        property: Atom,
        type_: Atom,
        data: Arc<Vec<u8>>,
    ) -> Result<()> {
        let len = data.len();
        if len > u32::MAX as usize {
            return self.send_selection_notify(request, 0);
        }

        // A new request for the same requestor/property supersedes any
        // unfinished INCR flow, whether the new reply is simple or INCR.
        self.transfers.retain(|transfer| {
            !(transfer.requestor == request.requestor && transfer.property == property)
        });

        if len > INCR_THRESHOLD {
            // Requestor property notifications are needed for the INCR flow
            // and for cancelling when the requestor is destroyed.
            let _ = xproto::change_window_attributes(
                &self.conn,
                request.requestor,
                &ChangeWindowAttributesAux::new()
                    .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
            );
            let size = if self.msb {
                (len as u32).to_be_bytes()
            } else {
                (len as u32).to_le_bytes()
            };
            xproto::change_property(
                &self.conn,
                PropMode::REPLACE,
                request.requestor,
                property,
                self.atoms.incr,
                32,
                1,
                &size,
            )
            .context("start INCR transfer")?;
            self.transfers.push(OutTransfer {
                requestor: request.requestor,
                property,
                type_,
                data,
                offset: 0,
                deadline: next_deadline(),
            });
        } else {
            xproto::change_property(
                &self.conn,
                PropMode::REPLACE,
                request.requestor,
                property,
                type_,
                8,
                len as u32,
                data.as_slice(),
            )
            .context("write selection property")?;
        }
        self.send_selection_notify(request, property)
    }

    fn advance_transfer(&mut self, index: usize) -> Result<()> {
        let (requestor, property, type_, offset, len, data) = {
            let transfer = &self.transfers[index];
            (
                transfer.requestor,
                transfer.property,
                transfer.type_,
                transfer.offset,
                transfer.data.len(),
                Arc::clone(&transfer.data),
            )
        };

        if offset < len {
            let end = len.min(offset.saturating_add(INCR_CHUNK));
            xproto::change_property(
                &self.conn,
                PropMode::REPLACE,
                requestor,
                property,
                type_,
                8,
                (end - offset) as u32,
                &data.as_slice()[offset..end],
            )
            .context("write INCR chunk")?;
            if let Some(transfer) = self.transfers.get_mut(index) {
                transfer.offset = end;
            }
        } else {
            // Zero-length final chunk.
            xproto::change_property(
                &self.conn,
                PropMode::REPLACE,
                requestor,
                property,
                type_,
                8,
                0,
                &[],
            )
            .context("finish INCR transfer")?;
            if index < self.transfers.len() {
                self.transfers.remove(index);
            }
        }
        Ok(())
    }

    fn send_selection_notify(
        &self,
        request: &xproto::SelectionRequestEvent,
        property: Atom,
    ) -> Result<()> {
        let event = xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property,
        };
        xproto::send_event(
            &self.conn,
            false,
            request.requestor,
            EventMask::NO_EVENT,
            event,
        )
        .context("send SelectionNotify")?;
        Ok(())
    }

    fn handle_command(&mut self, command: Command) -> Result<bool> {
        match command {
            Command::Read { id, serial, items } => self.command_read(id, serial, items)?,
            Command::CancelReads => self.fail_all_reads(),
            Command::Publish { guard, payload } => self.command_publish(guard, payload)?,
            Command::Stop => return Ok(true),
        }
        Ok(false)
    }

    fn command_publish(&mut self, guard: Guard, payload: Arc<Payload>) -> Result<()> {
        if !self.shared.valid(guard) {
            return Ok(());
        }
        let pending = self.build_publish(guard, payload)?;
        if self.targets_pending.is_some() {
            // A newer external copy is being resolved; never overwrite its
            // unknown data with an older forward.
            self.deferred_publish = Some(pending);
        } else {
            self.begin_publish(pending)?;
        }
        Ok(())
    }

    fn command_read(&mut self, id: u64, serial: u64, items: Vec<ReadItem>) -> Result<()> {
        if serial != self.latest_serial {
            self.emit(Event::ReadComplete {
                side: Side::X11,
                id,
                serial,
                data: None,
            });
            return Ok(());
        }

        if self.owner == self.window {
            let data = match self.own.as_ref() {
                Some(own) => {
                    let mut data = Vec::with_capacity(items.len());
                    let mut complete = true;
                    for item in &items {
                        match own.payload.data(&item.mime) {
                            Some(value) if value.len() <= item.limit => data.push(value),
                            _ => {
                                complete = false;
                                break;
                            }
                        }
                    }
                    if complete { Some(data) } else { None }
                }
                None => None,
            };
            self.emit(Event::ReadComplete {
                side: Side::X11,
                id,
                serial,
                data,
            });
            return Ok(());
        }

        if self.owner == 0 {
            self.emit(Event::ReadComplete {
                side: Side::X11,
                id,
                serial,
                data: None,
            });
            return Ok(());
        }

        if items.is_empty() {
            self.emit(Event::ReadComplete {
                side: Side::X11,
                id,
                serial,
                data: Some(Vec::new()),
            });
            return Ok(());
        }

        let mut job = ReadJob {
            id,
            serial,
            deadline: next_deadline(),
            items: Vec::with_capacity(items.len()),
        };

        for item in items {
            let target = match self.intern(&item.mime) {
                Ok(atom) => atom,
                Err(error) => {
                    for started in &job.items {
                        self.destroy_window(started.window);
                    }
                    return Err(error);
                }
            };
            let window = match self.new_window() {
                Ok(window) => window,
                Err(error) => {
                    for started in &job.items {
                        self.destroy_window(started.window);
                    }
                    return Err(error);
                }
            };
            if let Err(error) = xproto::convert_selection(
                &self.conn,
                window,
                self.atoms.clipboard,
                target,
                self.atoms.data_property,
                0u32,
            ) {
                self.destroy_window(window);
                for started in &job.items {
                    self.destroy_window(started.window);
                }
                return Err(error).context("request clipboard target");
            }
            job.items.push(IncomingItem {
                window,
                limit: item.limit,
                state: IncomingState::AwaitingNotify,
            });
        }

        self.reads.push(job);
        Ok(())
    }

    fn expire_deadlines(&mut self) -> Result<()> {
        let now = Instant::now();

        let targets_expired = self
            .targets_pending
            .as_ref()
            .is_some_and(|pending| now >= pending.deadline);
        if targets_expired && let Some(pending) = self.targets_pending.take() {
            self.destroy_window(pending.window);
            // Deadline expiry invalidates: no replay.
            self.had_selection = true;
            let epoch = self.shared.selection(Side::X11, pending.serial, &[]);
            self.emit(Event::Offer {
                side: Side::X11,
                serial: pending.serial,
                epoch,
                types: Vec::new(),
            });
        }

        let mut index = 0;
        while index < self.reads.len() {
            if now >= self.reads[index].deadline {
                self.fail_job(index);
            } else {
                index += 1;
            }
        }

        if self
            .publish_wait
            .as_ref()
            .is_some_and(|pending| now >= pending.deadline)
        {
            self.publish_wait = None;
        }

        let mut index = 0;
        while index < self.transfers.len() {
            if now >= self.transfers[index].deadline {
                let transfer = self.transfers.remove(index);
                // Signal the requestor that no more data is coming.
                self.delete_property(transfer.requestor, transfer.property);
            } else {
                index += 1;
            }
        }

        self.maybe_start_deferred_publish()?;
        Ok(())
    }

    fn shutdown(&mut self) {
        self.fail_all_reads();
        self.cancel_targets();
        self.publish_wait = None;
        self.deferred_publish = None;
        while let Some(transfer) = self.transfers.pop() {
            self.delete_property(transfer.requestor, transfer.property);
        }
        self.destroy_window(self.window);
        let _ = self.conn.flush();
    }
}

/// Run the X11 half of the clipboard bridge.
///
/// The function owns its connection and returns only after `Stop`, a closed
/// command channel, or an X11 connection failure.
pub fn run(commands: Commands, events: Sender<Event>, shared: Arc<Shared>) -> anyhow::Result<()> {
    let mut backend = Backend::init(commands, events, shared)?;
    backend.event_loop()
}

#[cfg(test)]
mod tests {
    use super::{IncrHint, append_incr_chunk, parse_incr_announcement};

    #[test]
    fn zero_element_incr_header_means_unknown_size() {
        // Stock xclip announces INCR with format32 and zero elements.
        assert_eq!(parse_incr_announcement(&[], false), Some(IncrHint::Unknown));
    }

    #[test]
    fn stored_zero_incr_header_means_unknown_size() {
        assert_eq!(
            parse_incr_announcement(&0u32.to_le_bytes(), false),
            Some(IncrHint::Unknown)
        );
    }

    #[test]
    fn incr_header_size_is_byte_order_aware() {
        assert_eq!(
            parse_incr_announcement(&0x0102_0304u32.to_le_bytes(), false),
            Some(IncrHint::Size(0x0102_0304))
        );
        assert_eq!(
            parse_incr_announcement(&0x0102_0304u32.to_be_bytes(), true),
            Some(IncrHint::Size(0x0102_0304))
        );
    }

    #[test]
    fn truncated_incr_header_is_malformed() {
        assert_eq!(parse_incr_announcement(&[1, 2, 3], false), None);
    }

    #[test]
    fn incr_payload_is_bounded_by_limit_not_advertised_hint() {
        // An advertised size is only a lower bound; chunks may accumulate
        // past it up to the configured limit.
        let Some(IncrHint::Size(hint)) = parse_incr_announcement(&4u32.to_le_bytes(), false) else {
            panic!("expected a numeric INCR hint");
        };
        let mut buffer = Vec::new();
        assert!(append_incr_chunk(&mut buffer, &[1, 2, 3, 4], 8));
        assert!(append_incr_chunk(&mut buffer, &[5, 6, 7, 8], 8));
        assert!(buffer.len() > hint);
        assert_eq!(buffer, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn incr_limit_failure_does_not_partially_append() {
        let mut buffer = vec![1, 2, 3, 4];
        assert!(!append_incr_chunk(&mut buffer, &[5, 6, 7, 8, 9], 8));
        assert_eq!(buffer, vec![1, 2, 3, 4]);
    }
}
