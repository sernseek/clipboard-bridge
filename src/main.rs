mod guest;
mod payload;
mod protocol;
mod wayland;
mod x11;

use anyhow::{Context, Result};
use guest::{Bundle, Guest, Target};
use payload::Format;
use protocol::{Command, Control, Event, Guard, MARKER, Payload, Shared, Side};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::thread;

struct Offer {
    serial: u64,
    types: Vec<String>,
}
enum ReadJob {
    Source {
        guard: Guard,
        format: Format,
    },
    Compare {
        guard: Guard,
        format: Format,
        payload: Arc<Payload>,
    },
    /// The latest copy, for the guest the input just moved to.
    Guest {
        guard: Guard,
        bundle: Bundle,
    },
}
struct Pending {
    side: Side,
    serial: u64,
    job: ReadJob,
}

fn coordinate(
    events: mpsc::Receiver<Event>,
    controls: &[Control; 2],
    shared: &Shared,
    guest: Option<&Guest>,
) -> Result<()> {
    let mut offers: [Option<Offer>; 2] = [None, None];
    let mut ready = [false; 2];
    let mut jobs = HashMap::with_capacity(2);
    let mut next_id = 0_u64;
    // The side and generation of the latest genuine copy, and the
    // generation the guest holds: it gets each copy once, never its own.
    let mut latest: Option<(Side, u64)> = None;
    let mut guest_has: Option<u64> = None;
    let mut target: Option<Target> = None;
    while let Ok(event) = events.recv() {
        match event {
            Event::Shutdown => return Ok(()),
            Event::Switch(to) => {
                let from = std::mem::replace(&mut target, to);
                let (Some(guest), Some(from), Some(to)) = (guest, from, to) else {
                    continue;
                };
                if from == to {
                    continue;
                }
                if to == Target::Host {
                    guest.fetch();
                    continue;
                }
                let Some((side, epoch)) = latest else {
                    continue;
                };
                if side == Side::Guest
                    || guest_has == Some(epoch)
                    || epoch != shared.epoch.load(Ordering::SeqCst)
                {
                    continue;
                }
                let Some(offer) = offers[side.index()].as_ref() else {
                    continue;
                };
                let Some(bundle) = Bundle::select(&offer.types) else {
                    continue;
                };
                next_id += 1;
                controls[side.index()].send(Command::Read {
                    id: next_id,
                    serial: offer.serial,
                    items: bundle.requests(),
                })?;
                let guard = Guard {
                    epoch,
                    source: side,
                    serial: offer.serial,
                };
                jobs.insert(
                    next_id,
                    Pending {
                        side,
                        serial: offer.serial,
                        job: ReadJob::Guest { guard, bundle },
                    },
                );
            }
            Event::GuestOffer(payload) => {
                // A copy like any other: newer than everything pending.
                let epoch = shared.epoch.fetch_add(1, Ordering::SeqCst) + 1;
                let serial = shared.serials[Side::Guest.index()].fetch_add(1, Ordering::SeqCst) + 1;
                let guard = Guard {
                    epoch,
                    source: Side::Guest,
                    serial,
                };
                jobs.clear();
                let payload = Arc::new(payload);
                for control in controls {
                    control.send(Command::CancelReads)?;
                    control.send(Command::Publish {
                        guard,
                        payload: Arc::clone(&payload),
                    })?;
                }
                latest = Some((Side::Guest, epoch));
                guest_has = Some(epoch);
            }
            Event::Failed { side, error } => anyhow::bail!("{side:?} backend: {error}"),
            Event::Ready(side) => {
                ready[side.index()] = true;
                if ready.iter().all(|ready| *ready) {
                    eprintln!("clipboard-bridge: watching native Wayland and X11 clipboards");
                }
            }
            Event::Offer {
                side,
                serial,
                epoch,
                types,
            } => {
                let own = types.iter().any(|mime| mime == MARKER);
                offers[side.index()] = Some(Offer { serial, types });
                // A delayed self-notification must not invalidate a genuine copy.
                if own || epoch != shared.epoch.load(Ordering::SeqCst) {
                    continue;
                }
                latest = Some((side, epoch));
                jobs.clear();
                for control in controls {
                    control.send(Command::CancelReads)?;
                }
                let Some(format) = Format::select(&offers[side.index()].as_ref().unwrap().types)
                else {
                    continue;
                };
                let guard = Guard {
                    epoch,
                    source: side,
                    serial,
                };
                next_id += 1;
                controls[side.index()].send(Command::Read {
                    id: next_id,
                    serial,
                    items: format.requests(),
                })?;
                jobs.insert(
                    next_id,
                    Pending {
                        side,
                        serial,
                        job: ReadJob::Source { guard, format },
                    },
                );
            }
            Event::ReadComplete {
                side,
                id,
                serial,
                data,
            } => {
                let Some(pending) = jobs.remove(&id) else {
                    continue;
                };
                if pending.side != side || pending.serial != serial {
                    continue;
                }
                match pending.job {
                    ReadJob::Guest { guard, bundle } => {
                        if !shared.valid(guard) {
                            continue;
                        }
                        let (Some(guest), Some(outgoing)) =
                            (guest, data.and_then(|values| bundle.normalize(values)))
                        else {
                            continue;
                        };
                        guest.push(outgoing);
                        guest_has = Some(guard.epoch);
                    }
                    ReadJob::Source { guard, format } => {
                        if !shared.valid(guard) {
                            continue;
                        }
                        let Some(payload) = data.and_then(|values| format.normalize(values)) else {
                            continue;
                        };
                        let payload = Arc::new(payload);
                        let destination = side.other();
                        let candidate = offers[destination.index()].as_ref().and_then(|offer| {
                            let other = Format::select(&offer.types)?;
                            format
                                .comparable(&other, &offer.types)
                                .then_some((offer.serial, other))
                        });
                        if let Some((serial, other)) = candidate {
                            next_id += 1;
                            controls[destination.index()].send(Command::Read {
                                id: next_id,
                                serial,
                                items: other.requests(),
                            })?;
                            jobs.insert(
                                next_id,
                                Pending {
                                    side: destination,
                                    serial,
                                    job: ReadJob::Compare {
                                        guard,
                                        format: other,
                                        payload,
                                    },
                                },
                            );
                        } else {
                            controls[destination.index()]
                                .send(Command::Publish { guard, payload })?;
                        }
                    }
                    ReadJob::Compare {
                        guard,
                        format,
                        payload,
                    } => {
                        if !shared.valid(guard) {
                            continue;
                        }
                        let equal = data
                            .and_then(|values| format.normalize(values))
                            .is_some_and(|existing| existing == *payload);
                        // Equal clipboard bytes need no new ownership. In particular,
                        // keep a native Wayland HTML/rich-text offer intact.
                        if !equal {
                            controls[side.index()].send(Command::Publish { guard, payload })?;
                        }
                    }
                }
            }
        }
    }
    anyhow::bail!("all backend event channels closed")
}

/// `--guest-port PORT [--guest-cid CID] [--switch-socket PATH]` adds a
/// passthrough guest; without it the bridge only joins the two displays.
fn guest_config() -> Result<Option<guest::Config>> {
    let mut port = None;
    let mut cid = 3;
    let mut switch_socket = "/run/kvm-switch/control.sock".into();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .with_context(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--guest-port" => port = Some(value.parse()?),
            "--guest-cid" => cid = value.parse()?,
            "--switch-socket" => switch_socket = value.into(),
            _ => anyhow::bail!("unknown argument {flag}"),
        }
    }
    Ok(port.map(|port| guest::Config {
        port,
        cid,
        switch_socket,
    }))
}

fn main() -> Result<()> {
    let guest_config = guest_config()?;
    // Native writes may target a consumer which closed its pipe early.
    // EPIPE is an individual transfer failure, never a process-wide signal.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let shared = Arc::new(Shared::default());
    let (events_tx, events_rx) = mpsc::channel();
    let mut signals = signal_hook::iterator::Signals::new([libc::SIGTERM, libc::SIGINT])?;
    let signals_handle = signals.handle();
    let signal_events = events_tx.clone();
    let signals_thread = thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            if signals.forever().next().is_some() {
                let _ = signal_events.send(Event::Shutdown);
            }
        })?;
    let (wayland_control, wayland_commands) = protocol::channel()?;
    let (x11_control, x11_commands) = protocol::channel()?;
    type Backend = fn(protocol::Commands, mpsc::Sender<Event>, Arc<Shared>) -> Result<()>;
    let mut workers = Vec::with_capacity(2);
    for (side, commands, run) in [
        (Side::Wayland, wayland_commands, wayland::run as Backend),
        (Side::X11, x11_commands, x11::run as Backend),
    ] {
        let sender = events_tx.clone();
        let state = Arc::clone(&shared);
        workers.push(
            thread::Builder::new()
                .name(format!("{side:?}"))
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run(commands, sender.clone(), state)
                    }));
                    let error = match result {
                        Ok(Err(error)) => format!("{error:#}"),
                        Ok(Ok(())) => "backend stopped".to_owned(),
                        Err(_) => "backend panicked".to_owned(),
                    };
                    let _ = sender.send(Event::Failed { side, error });
                })
                .with_context(|| format!("start {side:?} backend"))?,
        );
    }
    let guest = guest_config
        .map(|config| guest::spawn(config, events_tx.clone()))
        .transpose()?;
    drop(events_tx);
    let controls = [wayland_control, x11_control];
    let result = coordinate(events_rx, &controls, &shared, guest.as_ref());
    for control in &controls {
        let _ = control.send(Command::Stop);
    }
    for worker in workers {
        let _ = worker.join();
    }
    signals_handle.close();
    let _ = signals_thread.join();
    result
}
