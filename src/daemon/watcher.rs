/// Wayland clipboard watcher using zwlr_data_control_manager_v1.
///
/// For each clipboard change we enumerate all offered MIME types, read each one,
/// compute a hash over the full set of (mime, content) pairs, and store the entry.
/// This means "copy file" and "copy file path as text" produce distinct hashes
/// because their MIME type sets differ even if the text/plain content is identical.
use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::Utc;
use log::{debug, info, warn};
use sha2::{Digest, Sha256};
use wayland_client::{
    protocol::{wl_registry, wl_seat::WlSeat},
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
};

use mimeclip_common::common::db::{Database, NewEntry};
use mimeclip_common::common::types::{build_label, classify_kind};

use crate::suppress::SharedSuppressState;

const MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

/// MIME types we skip (portal file-transfer sessions, etc.)
const SKIP_MIME_PREFIXES: &[&str] = &[
    "application/vnd.portal.filetransfer",
    "application/vnd.portal.files",
];

struct PendingOffer {
    offer: ZwlrDataControlOfferV1,
    mime_types: Vec<String>,
}

struct WatchState {
    db: Arc<Mutex<Database>>,
    suppress_hash: SharedSuppressState,
    manager: Option<ZwlrDataControlManagerV1>,
    seat: Option<WlSeat>,
    device: Option<ZwlrDataControlDeviceV1>,
    /// Offers received but not yet selected, keyed by object id.
    pending_offers: HashMap<u32, Vec<String>>,
    /// Offer that was selected and needs to be read + stored.
    ready: Option<PendingOffer>,
}

impl WatchState {
    fn new(db: Arc<Mutex<Database>>, suppress_hash: SharedSuppressState) -> Self {
        Self {
            db,
            suppress_hash,
            manager: None,
            seat: None,
            device: None,
            pending_offers: HashMap::new(),
            ready: None,
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for WatchState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "zwlr_data_control_manager_v1" => {
                    state.manager = Some(registry.bind::<ZwlrDataControlManagerV1, _, _>(
                        name,
                        version.min(2),
                        qh,
                        (),
                    ));
                }
                "wl_seat" => {
                    if state.seat.is_none() {
                        state.seat =
                            Some(registry.bind::<WlSeat, _, _>(name, version.min(7), qh, ()));
                    }
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for WatchState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for WatchState {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wayland_client::protocol::wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for WatchState {
    fn event_created_child(
        opcode: u16,
        qhandle: &QueueHandle<Self>,
    ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
        match opcode {
            // data_offer event creates a new ZwlrDataControlOfferV1
            0 => qhandle.make_data::<ZwlrDataControlOfferV1, ()>(()),
            _ => panic!("unexpected child opcode {opcode} for ZwlrDataControlDeviceV1"),
        }
    }

    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                let oid = id.id().protocol_id();
                debug!("new data offer {oid}");
                state.pending_offers.insert(oid, Vec::new());
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                // Remove stale offers.
                let selected_oid = id.as_ref().map(|o| o.id().protocol_id());
                let selected_mimes = selected_oid.and_then(|oid| state.pending_offers.remove(&oid));
                state.pending_offers.clear();

                if let (Some(offer), Some(mime_types)) = (id, selected_mimes) {
                    if mime_types.is_empty() {
                        offer.destroy();
                    } else {
                        state.ready = Some(PendingOffer { offer, mime_types });
                    }
                }
            }
            zwlr_data_control_device_v1::Event::Finished => {
                warn!("data control device finished");
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for WatchState {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            let oid = offer.id().protocol_id();
            if let Some(list) = state.pending_offers.get_mut(&oid) {
                if !should_skip(&mime_type) {
                    list.push(mime_type);
                }
            }
        }
    }
}

fn should_skip(mime: &str) -> bool {
    SKIP_MIME_PREFIXES
        .iter()
        .any(|prefix| mime.starts_with(prefix))
}

/// Process a ready offer: issue receive() for each MIME type, flush, read, store.
/// Must be called from the main loop (not from within dispatch) so we can flush.
fn process_ready(
    pending: PendingOffer,
    db: &Arc<Mutex<Database>>,
    suppress_hash: &SharedSuppressState,
    event_queue: &mut EventQueue<WatchState>,
) {
    let PendingOffer { offer, mime_types } = pending;

    // Create pipes for all MIME types before issuing any receive().
    struct Pipe {
        mime: String,
        read: OwnedFd,
        write: OwnedFd,
    }

    let mut pipes: Vec<Pipe> = Vec::with_capacity(mime_types.len());

    for mime in &mime_types {
        match nix::unistd::pipe() {
            Ok((read, write)) => {
                offer.receive(mime.clone(), write.as_fd());
                pipes.push(Pipe {
                    mime: mime.clone(),
                    read,
                    write,
                });
            }
            Err(e) => {
                warn!("pipe() for {mime}: {e}");
            }
        }
    }

    // Flush: sends all queued receive() requests to the compositor.
    if let Err(e) = event_queue.flush() {
        warn!("flush: {e}");
        offer.destroy();
        return;
    }

    // Close write ends so reads don't block forever.
    let pipes: Vec<(String, OwnedFd)> = pipes
        .into_iter()
        .map(|p| {
            drop(p.write);
            (p.mime, p.read)
        })
        .collect();

    let mut payloads: Vec<(String, Vec<u8>)> = Vec::new();
    let mut total_size = 0usize;

    for (mime, read_fd) in pipes {
        let mut f = unsafe { std::fs::File::from_raw_fd(read_fd.into_raw_fd()) };
        let mut buf = Vec::new();
        match f.read_to_end(&mut buf) {
            Ok(_) => {}
            Err(e) => {
                warn!("read {mime}: {e}");
                continue;
            }
        }
        total_size += buf.len();
        if total_size > MAX_PAYLOAD_BYTES {
            warn!("entry exceeds 64 MiB cap, skipping");
            offer.destroy();
            return;
        }
        if !buf.is_empty() {
            payloads.push((mime, buf));
        }
    }

    offer.destroy();

    if payloads.is_empty() {
        return;
    }

    // Hash over sorted (mime, data) pairs — the key insight is that
    // (text/plain + x-special/gnome-copied-files + text/uri-list) hashes
    // differently from (text/plain) even with identical text content.
    let mut sorted = payloads.clone();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    for (mime, data) in &sorted {
        hasher.update(mime.as_bytes());
        hasher.update(b"\x00");
        hasher.update(data);
        hasher.update(b"\x00");
    }
    let hash = hex::encode(hasher.finalize());

    if suppress_hash.lock().unwrap().should_suppress(&hash) {
        info!("suppressed restored entry {hash}");
        return;
    }

    let kind = classify_kind(&mime_types);
    let label = build_label(&kind, &payloads);
    let preview = label.clone();
    let now = Utc::now();

    match db.lock().unwrap().insert(NewEntry {
        hash: &hash,
        kind: &kind,
        label: &label,
        preview: &preview,
        size: total_size,
        timestamp: now,
        payloads: &payloads,
    }) {
        Ok(id) => info!(
            "stored entry {id} [{kind_label}] {label}",
            kind_label = kind.label()
        ),
        Err(e) => warn!("db insert: {e}"),
    }
}

pub fn run(db: Arc<Mutex<Database>>, suppress_hash: SharedSuppressState) -> Result<()> {
    let conn = Connection::connect_to_env().context("connect to Wayland display")?;
    let mut event_queue: EventQueue<WatchState> = conn.new_event_queue();
    let qh = event_queue.handle();

    let display = conn.display();
    let _registry = display.get_registry(&qh, ());

    let mut state = WatchState::new(db, suppress_hash);

    // First roundtrip: receive globals.
    event_queue.roundtrip(&mut state)?;

    let manager = state
        .manager
        .as_ref()
        .context("compositor lacks zwlr_data_control_manager_v1")?;
    let seat = state.seat.as_ref().context("no wl_seat")?;

    let device = manager.get_data_device(seat, &qh, ());
    state.device = Some(device);

    // Second roundtrip: receive current clipboard selection if any.
    event_queue.roundtrip(&mut state)?;

    // Process initial clipboard content if present.
    if let Some(ready) = state.ready.take() {
        let db = state.db.clone();
        let suppress_hash = state.suppress_hash.clone();
        process_ready(ready, &db, &suppress_hash, &mut event_queue);
    }

    info!("mimeclipd watching clipboard");

    loop {
        event_queue.blocking_dispatch(&mut state)?;

        if let Some(ready) = state.ready.take() {
            let db = state.db.clone();
            let suppress_hash = state.suppress_hash.clone();
            process_ready(ready, &db, &suppress_hash, &mut event_queue);
        }
    }
}
