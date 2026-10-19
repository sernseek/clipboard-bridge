//! Native Wayland clipboard backend built on `wlr-data-control-unstable-v1`.
//!
//! This module owns the `Wayland` side of the bridge without spawning any
//! clipboard helper process. It binds `zwlr_data_control_manager_v1` (version
//! `min(server, 2)`), creates one data-control device for the first advertised
//! seat, mirrors that seat's regular selection into the coordinator and serves
//! the selections the coordinator publishes.
//!
//! Edge handling that matters in practice:
//!
//! * A compositor without a clipboard owner at startup produces a `selection`
//!   event with a null offer. That initial null is ignored completely: it must
//!   not advance [`Shared::epoch`], otherwise the other backend's first real
//!   copy could be invalidated while initialization is still racing.
//! * Every real selection (including an offer that only carries our origin
//!   marker) receives a fresh monotonic backend-local serial. Marker offers do
//!   not advance the shared epoch; an explicit clear after a real selection
//!   does.
//! * Offers are tracked from `data_offer` until the matching `selection` or
//!   `primary_selection` event adopts them. Offers superseded by a newer
//!   selection/primary offer are destroyed; offers that were introduced but
//!   never adopted are destroyed after the following announcement, which keeps
//!   a broken compositor from leaking objects without destroying an offer that
//!   belongs to the second half of the same clipboard change.
//! * Incoming reads use `pipe2(O_CLOEXEC)` with only the read side
//!   non-blocking. While a source delivers data slowly the full Wayland queue,
//!   the command wake socket and the outgoing transfers keep being serviced.
//! * Outgoing transfers write from the shared `Arc<Payload>` buffers directly
//!   with partial, non-blocking writes. A consumer that closes its pipe, a
//!   full pipe or an expired 5-second transfer deadline only drops that
//!   transfer; it never terminates the process or the owning selection.
//! * Transfers that were already started keep their payload alive after a new
//!   publication replaces ours, so an in-flight paste is not truncated.
//!
//! Only the wlr data-control protocol is used. Compositors that only implement
//! `ext-data-control-v1` (for example older GNOME builds) are rejected with a
//! descriptive error instead of silently missing the clipboard.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::mpsc::{Sender, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::event_created_child;
use wayland_client::globals::{BindError, GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self as device, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::{self as manager, ZwlrDataControlManagerV1},
    zwlr_data_control_offer_v1::{self as offer, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self as source, ZwlrDataControlSourceV1},
};

use crate::protocol::{
    Command, Commands, DEADLINE, Event, Guard, MARKER, Payload, ReadItem, Shared, Side,
};

/// Upper bound on concurrently served outgoing transfers. Beyond this the
/// compositor-supplied file descriptor is closed immediately, which makes the
/// consumer observe an empty response instead of unbounded descriptor growth.
const MAX_TRANSFERS: usize = 16;
const READ_CHUNK: usize = 64 * 1024;
/// Bytes read between command/event pumps while a producer keeps the pipe
/// readable. Bounds how long a queued `CancelReads` can wait without slowing
/// the read itself.
const READ_BUDGET: usize = 4 * READ_CHUNK;
/// wayland fd + command wake fd + optional read fd + every possible transfer.
const MAX_POLL_FDS: usize = 3 + MAX_TRANSFERS;

// ---------------------------------------------------------------------------
// Protocol state
// ---------------------------------------------------------------------------

struct Offer {
    proxy: ZwlrDataControlOfferV1,
    mimes: Vec<String>,
}

struct Selection {
    proxy: ZwlrDataControlOfferV1,
    mimes: Vec<String>,
    serial: u64,
}

struct Primary {
    proxy: ZwlrDataControlOfferV1,
}

struct Own {
    source: ObjectId,
    payload: Arc<Payload>,
    serial: u64,
    epoch: u64,
}

struct Source {
    proxy: ZwlrDataControlSourceV1,
    payload: Arc<Payload>,
}

struct Transfer {
    fd: OwnedFd,
    data: Arc<Vec<u8>>,
    offset: usize,
    deadline: Instant,
}

struct State {
    events: Sender<Event>,
    shared: Arc<Shared>,
    qh: QueueHandle<State>,
    manager: ZwlrDataControlManagerV1,
    seat: wl_seat::WlSeat,
    device: ZwlrDataControlDeviceV1,
    manager_global: u32,
    seat_global: u32,
    serial: u64,
    selection_serial: Option<u64>,
    selection: Option<Selection>,
    primary: Option<Primary>,
    offers: HashMap<ObjectId, Offer>,
    /// Offers introduced by `data_offer` that no announcement has adopted yet,
    /// mapped to the announcement count at the time they were created.
    pending: HashMap<ObjectId, u64>,
    announce: u64,
    own: Option<Own>,
    sources: HashMap<ObjectId, Source>,
    transfers: Vec<Transfer>,
    /// Incremented whenever the regular selection really changes. An
    /// in-progress read compares against the value it started with.
    selection_generation: u64,
    /// True once any non-null selection or publication has been observed.
    had_selection: bool,
    fatal: Option<String>,
    stopping: bool,
    cancel_reads: bool,
    deferred: VecDeque<(u64, u64, Vec<ReadItem>)>,
}

impl State {
    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    fn fail(&mut self, message: impl Into<String>) {
        if self.fatal.is_none() {
            self.fatal = Some(message.into());
        }
    }

    fn next_serial(&mut self) -> u64 {
        self.serial = self.serial.wrapping_add(1);
        if self.serial == 0 {
            self.serial = 1;
        }
        self.serial
    }

    /// Whether an offer's MIME set is exactly what we published. This is only
    /// used to recognize our own marker echo; the cached payload is used only
    /// while that source is still alive (see `sources`).
    fn own_matches(&self, types: &[String]) -> bool {
        let Some(own) = &self.own else { return false };
        let payload = &own.payload;
        payload.mimes().count() == types.len()
            && types
                .iter()
                .all(|mime| payload.mimes().any(|candidate| candidate == mime.as_str()))
            && payload
                .mimes()
                .all(|mime| types.iter().any(|candidate| candidate.as_str() == mime))
    }

    /// Destroy offers that were introduced but never adopted by the
    /// announcement one step before the current one. An offer created for the
    /// second half of the same change (for example the primary-selection offer
    /// preceding `primary_selection`) is still kept.
    fn sweep_pending(&mut self, keep: Option<ObjectId>) {
        let stale: Vec<ObjectId> = self
            .pending
            .iter()
            .filter(|(id, created)| {
                Some((*id).clone()) != keep && created.saturating_add(1) < self.announce
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.pending.remove(&id);
            if let Some(offer) = self.offers.remove(&id) {
                offer.proxy.destroy();
            }
        }
    }

    fn on_data_offer(&mut self, proxy: ZwlrDataControlOfferV1) {
        let id = proxy.id();
        self.pending.insert(id.clone(), self.announce);
        self.offers.insert(
            id,
            Offer {
                proxy,
                mimes: Vec::new(),
            },
        );
    }

    fn on_selection(&mut self, id: Option<ZwlrDataControlOfferV1>) {
        self.announce += 1;
        if let Some(previous) = self.selection.take() {
            previous.proxy.destroy();
        }
        let keep = id.as_ref().map(Proxy::id);
        self.sweep_pending(keep);

        let Some(proxy) = id else {
            if !self.had_selection {
                // Startup with no clipboard owner: do not touch the shared
                // epoch, do not emit a synthetic empty offer.
                return;
            }
            let serial = self.next_serial();
            let epoch = self.shared.selection(Side::Wayland, serial, &[]);
            self.selection_serial = Some(serial);
            self.selection_generation += 1;
            self.emit(Event::Offer {
                side: Side::Wayland,
                serial,
                epoch,
                types: Vec::new(),
            });
            return;
        };

        let offer_id = proxy.id();
        self.pending.remove(&offer_id);
        let (proxy, types) = match self.offers.remove(&offer_id) {
            Some(offer) => (offer.proxy, offer.mimes),
            // The compositor announced an offer that we never saw. Keep it and
            // treat it as empty rather than losing the selection entirely.
            None => {
                let offer = Offer {
                    proxy: proxy.clone(),
                    mimes: Vec::new(),
                };
                (offer.proxy, offer.mimes)
            }
        };

        self.had_selection = true;
        let echo = types.iter().any(|mime| mime == MARKER)
            && self
                .own
                .as_ref()
                .is_some_and(|own| self.own_matches(&types) && sources_has(&self.sources, own));
        let serial = match (&self.own, echo) {
            (Some(own), true) => own.serial,
            _ => self.next_serial(),
        };
        let epoch = self.shared.selection(Side::Wayland, serial, &types);
        if echo {
            if let Some(own) = &mut self.own {
                own.epoch = epoch;
            }
        } else {
            self.selection_generation += 1;
        }
        self.selection_serial = Some(serial);
        self.selection = Some(Selection {
            proxy,
            mimes: types.clone(),
            serial,
        });
        self.emit(Event::Offer {
            side: Side::Wayland,
            serial,
            epoch,
            types,
        });
    }

    fn on_primary(&mut self, id: Option<ZwlrDataControlOfferV1>) {
        self.announce += 1;
        if let Some(previous) = self.primary.take() {
            previous.proxy.destroy();
        }
        let keep = id.as_ref().map(Proxy::id);
        self.sweep_pending(keep);
        let Some(proxy) = id else { return };
        let offer_id = proxy.id();
        self.pending.remove(&offer_id);
        // The MIME list is not consulted for primary selection, but the object
        // is kept alive until it is superseded so it can be destroyed in the
        // order the protocol expects.
        self.offers.remove(&offer_id);
        self.primary = Some(Primary { proxy });
    }

    fn cancelled(&mut self, id: ObjectId) {
        if let Some(source) = self.sources.remove(&id) {
            source.proxy.destroy();
        }
        let own_serial = self
            .own
            .as_ref()
            .filter(|own| own.source == id)
            .map(|own| own.serial);
        let Some(own_serial) = own_serial else { return };
        // This publication is no longer the selection source. The cached
        // payload must not be used any more even if a delayed marker offer for
        // it is delivered; reads fall back to the actual current offer.
        self.own = None;
        if self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.serial == own_serial)
            && let Some(selection) = self.selection.take()
        {
            selection.proxy.destroy();
        }
        if self.selection_serial == Some(own_serial) {
            self.selection_serial = None;
        }
        self.selection_generation += 1;
    }

    fn serve(&mut self, source_id: ObjectId, mime: String, fd: OwnedFd) {
        let Some(source) = self.sources.get(&source_id) else {
            return;
        };
        let Some(data) = source.payload.data(&mime) else {
            // Unknown MIME: closing the descriptor is the correct empty answer.
            return;
        };
        if self.transfers.len() >= MAX_TRANSFERS || set_nonblocking(&fd).is_err() {
            return;
        }
        self.transfers.push(Transfer {
            fd,
            data,
            offset: 0,
            deadline: Instant::now() + DEADLINE,
        });
    }

    fn service_transfers(&mut self) {
        let mut index = 0;
        while index < self.transfers.len() {
            let finished = {
                let transfer = &mut self.transfers[index];
                if transfer.offset >= transfer.data.len() {
                    true
                } else {
                    let remaining = &transfer.data[transfer.offset..];
                    match write_fd(transfer.fd.as_raw_fd(), remaining) {
                        Ok(0) => true,
                        Ok(written) => {
                            transfer.offset += written;
                            transfer.offset >= transfer.data.len()
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => false,
                        // EPIPE (consumer closed early) and every other write
                        // error only end this transfer.
                        Err(_) => true,
                    }
                }
            };
            if finished {
                self.transfers.swap_remove(index);
            } else {
                index += 1;
            }
        }
        self.expire_transfers();
    }

    fn expire_transfers(&mut self) {
        let now = Instant::now();
        self.transfers.retain(|transfer| transfer.deadline > now);
    }

    fn teardown(&mut self) {
        if let Some(selection) = self.selection.take() {
            selection.proxy.destroy();
        }
        if let Some(primary) = self.primary.take() {
            primary.proxy.destroy();
        }
        for (_, offer) in self.offers.drain() {
            offer.proxy.destroy();
        }
        self.pending.clear();
        for (_, source) in self.sources.drain() {
            source.proxy.destroy();
        }
        self.transfers.clear();
        self.device.destroy();
        self.manager.destroy();
        if self.seat.version() >= 5 {
            self.seat.release();
        }
    }
}

fn sources_has(sources: &HashMap<ObjectId, Source>, own: &Own) -> bool {
    sources.contains_key(&own.source)
}

// ---------------------------------------------------------------------------
// Dispatch implementations
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::GlobalRemove { name } = event {
            if name == state.manager_global {
                state.fail("the compositor removed the zwlr_data_control_manager_v1 global");
            } else if name == state.seat_global {
                state.fail("the compositor removed the wl_seat global");
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_seat::WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Seat name/capability changes do not affect data-control selection.
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlManagerV1,
        _event: manager::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // The manager has no events.
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlDeviceV1,
        event: device::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            device::Event::DataOffer { id } => state.on_data_offer(id),
            device::Event::Selection { id } => state.on_selection(id),
            device::Event::PrimarySelection { id } => state.on_primary(id),
            device::Event::Finished => {
                state.fail("the compositor finished the data-control device")
            }
            _ => {}
        }
    }

    event_created_child!(State, ZwlrDataControlDeviceV1, [
        device::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrDataControlOfferV1,
        event: offer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let offer::Event::Offer { mime_type } = event
            && let Some(offer) = state.offers.get_mut(&proxy.id())
            && !offer.mimes.iter().any(|mime| mime == &mime_type)
        {
            offer.mimes.push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrDataControlSourceV1,
        event: source::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            source::Event::Send { mime_type, fd } => state.serve(proxy.id(), mime_type, fd),
            source::Event::Cancelled => state.cancelled(proxy.id()),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Ready {
    wayland: bool,
    wake: bool,
    data: bool,
    transfer: bool,
    timeout: bool,
}

struct Backend {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    commands: Commands,
    shared: Arc<Shared>,
    events: Sender<Event>,
    flush_blocked: bool,
    reading: bool,
}

impl Backend {
    fn emit(&self, event: Event) -> Result<()> {
        self.events
            .send(event)
            .map_err(|_| anyhow!("the coordinator stopped receiving backend events"))
    }

    fn check_fatal(&self) -> Result<()> {
        match &self.state.fatal {
            Some(message) => Err(anyhow!("{message}")),
            None => Ok(()),
        }
    }

    fn flush_once(&mut self) -> Result<()> {
        match self.connection.flush() {
            Ok(()) => {
                self.flush_blocked = false;
                Ok(())
            }
            Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {
                // The socket buffer is full; the event loop adds POLLOUT for
                // the Wayland fd and retries.
                self.flush_blocked = true;
                Ok(())
            }
            Err(error) => Err(anyhow!("cannot flush the Wayland connection: {error}")),
        }
    }

    /// Dispatch whatever became ready, wait for one of the descriptors and
    /// service the Wayland socket. Returns which sources were ready. Protocol
    /// events are always dispatched before the caller drains commands.
    fn poll_ready(&mut self, data_fd: Option<RawFd>, deadline: Option<Instant>) -> Result<Ready> {
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|error| anyhow!("cannot dispatch Wayland events: {error}"))?;
        if self.state.fatal.is_some() {
            return Ok(Ready::default());
        }

        self.flush_once()?;

        let mut guard = self.connection.prepare_read();
        let now = Instant::now();
        let mut until = deadline;
        for transfer in &self.state.transfers {
            until = Some(match until {
                Some(current) => current.min(transfer.deadline),
                None => transfer.deadline,
            });
        }
        let wait = until.map(|until| until.saturating_duration_since(now));

        let wayland_fd = guard
            .as_ref()
            .map(|guard| guard.connection_fd().as_raw_fd());
        let wayland_events = libc::POLLIN as libc::c_short
            | if self.flush_blocked {
                libc::POLLOUT as libc::c_short
            } else {
                0
            };
        // Fixed stack storage: the event loop must not allocate on every wake.
        let mut fds = [libc::pollfd {
            fd: -1,
            events: 0,
            revents: 0,
        }; MAX_POLL_FDS];
        let mut used = 0usize;
        let wayland_index = match wayland_fd {
            Some(fd) => {
                fds[used] = libc::pollfd {
                    fd,
                    events: wayland_events,
                    revents: 0,
                };
                used += 1;
                Some(used - 1)
            }
            None => None,
        };
        fds[used] = libc::pollfd {
            fd: self.commands.wake.as_raw_fd(),
            events: libc::POLLIN as libc::c_short,
            revents: 0,
        };
        let wake_index = used;
        used += 1;
        let data_index = match data_fd {
            Some(fd) => {
                fds[used] = libc::pollfd {
                    fd,
                    events: libc::POLLIN as libc::c_short,
                    revents: 0,
                };
                used += 1;
                Some(used - 1)
            }
            None => None,
        };
        let transfer_start = used;
        let mut transfer_count = 0usize;
        for transfer in &self.state.transfers {
            if used == MAX_POLL_FDS {
                break;
            }
            fds[used] = libc::pollfd {
                fd: transfer.fd.as_raw_fd(),
                events: libc::POLLOUT as libc::c_short,
                revents: 0,
            };
            used += 1;
            transfer_count += 1;
        }

        let count = poll_fds(&mut fds[..used], wait)?;
        let mut ready = Ready::default();
        if count == 0 {
            ready.timeout = true;
        } else {
            if let Some(index) = wayland_index {
                let revents = fds[index].revents;
                if revents & (libc::POLLOUT as libc::c_short) != 0 && self.flush_blocked {
                    self.flush_once()?;
                }
                if revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) as libc::c_short != 0 {
                    if let Some(guard) = guard.take() {
                        match guard.read() {
                            Ok(_) => {}
                            Err(WaylandError::Io(error))
                                if error.kind() == io::ErrorKind::WouldBlock => {}
                            Err(error) => {
                                return Err(anyhow!(
                                    "cannot read from the Wayland server: {error}"
                                ));
                            }
                        }
                    }
                    self.queue
                        .dispatch_pending(&mut self.state)
                        .map_err(|error| anyhow!("cannot dispatch Wayland events: {error}"))?;
                    ready.wayland = true;
                }
            }
            if fds[wake_index].revents
                & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) as libc::c_short
                != 0
            {
                ready.wake = true;
            }
            if let Some(index) = data_index
                && fds[index].revents
                    & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) as libc::c_short
                    != 0
            {
                ready.data = true;
            }
            if fds[transfer_start..transfer_start + transfer_count]
                .iter()
                .any(|fd| {
                    fd.revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) as libc::c_short
                        != 0
                })
            {
                ready.transfer = true;
            }
        }
        Ok(ready)
    }

    fn execute(&mut self, command: Command, nested: bool) -> Result<()> {
        match command {
            Command::Stop => {
                self.state.stopping = true;
                self.state.cancel_reads = true;
                self.fail_deferred();
            }
            Command::CancelReads => {
                self.fail_deferred();
                if self.reading {
                    self.state.cancel_reads = true;
                }
            }
            Command::Publish { guard, payload } => self.publish(guard, payload)?,
            Command::Read { id, serial, items } => {
                if nested {
                    self.state.deferred.push_back((id, serial, items));
                } else {
                    self.perform_read(id, serial, items)?;
                }
            }
        }
        Ok(())
    }

    fn run_commands(&mut self) -> Result<()> {
        loop {
            if let Some((id, serial, items)) = self.state.deferred.pop_front() {
                self.perform_read(id, serial, items)?;
                continue;
            }
            match self.commands.receiver.try_recv() {
                Ok(command) => self.execute(command, false)?,
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => {
                    self.state.stopping = true;
                    return Ok(());
                }
            }
            if self.state.stopping {
                self.fail_deferred();
                return Ok(());
            }
        }
    }

    /// Commands that arrive while a read is in progress. Reads are deferred so
    /// the current transfer cannot be re-entered.
    fn run_nested_commands(&mut self) -> Result<()> {
        loop {
            match self.commands.receiver.try_recv() {
                Ok(command) => self.execute(command, true)?,
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => {
                    self.state.stopping = true;
                    return Ok(());
                }
            }
            if self.state.stopping {
                return Ok(());
            }
        }
    }

    fn fail_deferred(&mut self) {
        while let Some((id, serial, _items)) = self.state.deferred.pop_front() {
            let _ = self.events.send(Event::ReadComplete {
                side: Side::Wayland,
                id,
                serial,
                data: None,
            });
        }
    }

    fn publish(&mut self, guard: Guard, payload: Arc<Payload>) -> Result<()> {
        if !self.shared.valid(guard) {
            // A newer selection won the race; publishing now would resurrect
            // stale clipboard data.
            return Ok(());
        }
        let source = self.state.manager.create_data_source(&self.state.qh, ());
        let types: Vec<String> = payload.mimes().map(str::to_owned).collect();
        for mime in &types {
            source.offer(mime.clone());
        }
        self.state.device.set_selection(Some(&source));

        let serial = self.state.next_serial();
        let epoch = self.shared.selection(Side::Wayland, serial, &types);
        let source_id = source.id();
        self.state.sources.insert(
            source_id.clone(),
            Source {
                proxy: source,
                payload: Arc::clone(&payload),
            },
        );
        self.state.own = Some(Own {
            source: source_id,
            payload,
            serial,
            epoch,
        });
        self.state.had_selection = true;
        self.state.selection_serial = Some(serial);
        self.state.selection_generation += 1;
        self.emit(Event::Offer {
            side: Side::Wayland,
            serial,
            epoch,
            types,
        })?;
        self.flush_once()
    }

    fn perform_read(&mut self, id: u64, serial: u64, items: Vec<ReadItem>) -> Result<()> {
        self.reading = true;
        let data = self.read_items(serial, &items);
        self.reading = false;
        self.state.cancel_reads = false;
        self.emit(Event::ReadComplete {
            side: Side::Wayland,
            id,
            serial,
            data,
        })
    }

    fn read_items(&mut self, serial: u64, items: &[ReadItem]) -> Option<Vec<Arc<Vec<u8>>>> {
        if self.state.selection_serial != Some(serial) {
            return None;
        }
        if let Some(own) = &self.state.own
            && own.serial == serial
            && sources_has(&self.state.sources, own)
        {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                let data = own.payload.data(&item.mime)?;
                if data.len() > item.limit {
                    return None;
                }
                values.push(data);
            }
            return Some(values);
        }

        let (proxy, mimes) = {
            let selection = self
                .state
                .selection
                .as_ref()
                .filter(|selection| selection.serial == serial)?;
            if !selection.proxy.is_alive() {
                return None;
            }
            (selection.proxy.clone(), selection.mimes.clone())
        };
        if items
            .iter()
            .any(|item| !mimes.iter().any(|mime| mime == &item.mime))
        {
            return None;
        }

        let deadline = Instant::now() + DEADLINE;
        let generation = self.state.selection_generation;
        let mut values = Vec::with_capacity(items.len());
        for item in items {
            if Instant::now() >= deadline {
                return None;
            }
            values.push(self.read_offer(&proxy, &item.mime, item.limit, deadline, generation)?);
        }
        Some(values)
    }

    fn read_offer(
        &mut self,
        proxy: &ZwlrDataControlOfferV1,
        mime: &str,
        limit: usize,
        deadline: Instant,
        generation: u64,
    ) -> Option<Arc<Vec<u8>>> {
        let (read_end, write_end) = pipe().ok()?;
        // Only the read side is non-blocking; the source keeps normal blocking
        // write semantics on its side of the pipe.
        set_nonblocking(&read_end).ok()?;
        proxy.receive(mime.to_owned(), write_end.as_fd());
        drop(write_end);
        self.flush_once().ok()?;

        let mut data: Vec<u8> = Vec::new();
        let mut chunk = [0u8; READ_CHUNK];
        let mut since_pump = 0usize;
        let mut chunks_since_pump = 0usize;
        loop {
            if self.state.fatal.is_some()
                || self.state.cancel_reads
                || self.state.stopping
                || self.state.selection_generation != generation
            {
                return None;
            }
            match read_fd(read_end.as_raw_fd(), &mut chunk) {
                Ok(0) => break,
                Ok(count) => {
                    if count > limit.saturating_sub(data.len()) {
                        return None;
                    }
                    data.extend_from_slice(&chunk[..count]);
                    since_pump += count;
                    chunks_since_pump += 1;
                    // A producer that never blocks must not hide the absolute
                    // deadline or a queued CancelReads. Both are serviced
                    // between bounded chunks, using the same poll path as a
                    // blocked read (no sleeps or fixed polling).
                    if Instant::now() >= deadline {
                        return None;
                    }
                    if since_pump >= READ_BUDGET || chunks_since_pump >= 8 {
                        since_pump = 0;
                        chunks_since_pump = 0;
                        if !self.wait_for_data(read_end.as_raw_fd(), deadline, generation) {
                            return None;
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    since_pump = 0;
                    chunks_since_pump = 0;
                    if !self.wait_for_data(read_end.as_raw_fd(), deadline, generation) {
                        return None;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
        }
        // Wrap the exact read buffer; no image-sized copy or conversion.
        Some(Arc::new(data))
    }

    /// Wait until the incoming pipe is readable. Wayland events, commands and
    /// outgoing transfers continue to be serviced meanwhile, so reading our own
    /// selection (same-process `send` event) also makes progress.
    fn wait_for_data(&mut self, data_fd: RawFd, deadline: Instant, generation: u64) -> bool {
        loop {
            if self.state.fatal.is_some()
                || self.state.cancel_reads
                || self.state.stopping
                || self.state.selection_generation != generation
                || Instant::now() >= deadline
            {
                return false;
            }
            let ready = match self.poll_ready(Some(data_fd), Some(deadline)) {
                Ok(ready) => ready,
                Err(error) => {
                    self.state.fail(error.to_string());
                    return false;
                }
            };
            self.state.service_transfers();
            if self.state.fatal.is_some() {
                return false;
            }
            if ready.wake {
                if let Err(error) = self.commands.drain_wake() {
                    self.state
                        .fail(format!("cannot drain command wake socket: {error}"));
                    return false;
                }
                if let Err(error) = self.run_nested_commands() {
                    self.state.fail(error.to_string());
                    return false;
                }
                if self.state.cancel_reads || self.state.stopping {
                    return false;
                }
            }
            if ready.data {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
        }
    }

    fn drive(&mut self) -> Result<()> {
        loop {
            if self.state.stopping {
                break;
            }
            let ready = self.poll_ready(None, None)?;
            self.check_fatal()?;
            if ready.wake {
                self.commands
                    .drain_wake()
                    .context("cannot drain the command wake socket")?;
                self.run_commands()?;
                if self.state.stopping {
                    break;
                }
            }
            if ready.transfer || ready.timeout {
                self.state.expire_transfers();
            }
            self.state.service_transfers();
            self.flush_once()?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(commands: Commands, events: Sender<Event>, shared: Arc<Shared>) -> Result<()> {
    let connection = Connection::connect_to_env().context(
        "cannot connect to the Wayland compositor (check WAYLAND_DISPLAY and XDG_RUNTIME_DIR)",
    )?;
    let (globals, queue) =
        registry_queue_init::<State>(&connection).context("cannot initialize Wayland globals")?;
    let qh = queue.handle();

    let manager_version = ZwlrDataControlManagerV1::interface().version.min(2);
    let manager = globals
        .bind::<ZwlrDataControlManagerV1, State, ()>(&qh, 1..=manager_version, ())
        .map_err(|error| match error {
            BindError::NotPresent => anyhow!(
                "the Wayland compositor does not advertise zwlr_data_control_manager_v1; \
                 the native backend requires the wlr-data-control protocol (Sway, Niri, \
                 Hyprland, labwc and other wlroots-style compositors)"
            ),
            BindError::UnsupportedVersion => anyhow!(
                "the Wayland compositor does not support version 1 of \
                 zwlr_data_control_manager_v1"
            ),
        })?;

    let seat_version = wl_seat::WlSeat::interface().version;
    let seat = globals
        .bind::<wl_seat::WlSeat, State, ()>(&qh, 1..=seat_version, ())
        .map_err(|error| match error {
            BindError::NotPresent => {
                anyhow!("the Wayland compositor did not advertise a wl_seat")
            }
            BindError::UnsupportedVersion => {
                anyhow!("the Wayland compositor's wl_seat does not support version 1")
            }
        })?;

    let manager_global = global_name(&globals, "zwlr_data_control_manager_v1")
        .ok_or_else(|| anyhow!("cannot locate the zwlr_data_control_manager_v1 global"))?;
    let seat_global = global_name(&globals, "wl_seat")
        .ok_or_else(|| anyhow!("cannot locate the wl_seat global"))?;

    let device = manager.get_data_device(&seat, &qh, ());
    let state = State {
        events: events.clone(),
        shared: Arc::clone(&shared),
        qh,
        manager,
        seat,
        device,
        manager_global,
        seat_global,
        serial: 0,
        selection_serial: None,
        selection: None,
        primary: None,
        offers: HashMap::new(),
        pending: HashMap::new(),
        announce: 0,
        own: None,
        sources: HashMap::new(),
        transfers: Vec::new(),
        selection_generation: 0,
        had_selection: false,
        fatal: None,
        stopping: false,
        cancel_reads: false,
        deferred: VecDeque::new(),
    };
    let mut backend = Backend {
        connection,
        queue,
        state,
        commands,
        shared,
        events,
        flush_blocked: false,
        reading: false,
    };

    backend.emit(Event::Ready(Side::Wayland))?;
    // The first selection event (possibly a null offer) is delivered by this
    // round trip. A null offer at this point does not touch the shared epoch.
    backend
        .queue
        .roundtrip(&mut backend.state)
        .map_err(|error| anyhow!("cannot initialize the Wayland clipboard device: {error}"))?;

    let outcome = backend.drive();
    backend.fail_deferred();
    backend.state.teardown();
    let _ = backend.connection.flush();
    outcome
}

fn global_name(globals: &GlobalList, interface: &str) -> Option<u32> {
    globals.contents().with_list(|list| {
        list.iter()
            .find(|global| global.interface == interface)
            .map(|g| g.name)
    })
}

// ---------------------------------------------------------------------------
// File descriptor helpers
// ---------------------------------------------------------------------------

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe2 returned two fresh descriptors that we exclusively own.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_fd(fd: RawFd, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        let count = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        return Ok(count as usize);
    }
}

fn write_fd(fd: RawFd, bytes: &[u8]) -> io::Result<usize> {
    loop {
        // Rust ignores SIGPIPE for the standard runtime, so a consumer closing
        // its end surfaces as EPIPE here instead of killing the process.
        let count = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        return Ok(count as usize);
    }
}

fn poll_fds(fds: &mut [libc::pollfd], wait: Option<Duration>) -> io::Result<usize> {
    let mut remaining = wait;
    loop {
        let timeout = match remaining {
            None => -1,
            Some(duration) if duration.is_zero() => 0,
            Some(duration) => {
                duration.as_millis().saturating_add(1).min(i32::MAX as u128) as libc::c_int
            }
        };
        let started = Instant::now();
        let count = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if count >= 0 {
            return Ok(count as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
        if let Some(duration) = remaining {
            remaining = Some(duration.saturating_sub(started.elapsed()));
        }
    }
}
