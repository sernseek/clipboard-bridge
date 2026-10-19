mod payload;
mod protocol;
mod wayland;
mod x11;

use anyhow::{Context, Result};
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
) -> Result<()> {
    let mut offers: [Option<Offer>; 2] = [None, None];
    let mut ready = [false; 2];
    let mut jobs = HashMap::with_capacity(2);
    let mut next_id = 0_u64;
    while let Ok(event) = events.recv() {
        match event {
            Event::Shutdown => return Ok(()),
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

fn main() -> Result<()> {
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
    drop(events_tx);
    let controls = [wayland_control, x11_control];
    let result = coordinate(events_rx, &controls, &shared);
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
