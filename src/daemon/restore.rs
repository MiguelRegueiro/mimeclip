/// Restore a clipboard entry by re-offering all its MIME types simultaneously.
///
/// We become a Wayland data source, offer all stored MIME types, and serve data
/// until the source is cancelled (something pasted or clipboard was replaced).
use std::collections::HashMap;
use std::io::Write;
use std::os::fd::OwnedFd;

use anyhow::{Context, Result};
use log::{debug, warn};
use wayland_client::{
    protocol::{wl_registry, wl_seat::WlSeat},
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

struct RestoreState {
    manager: Option<ZwlrDataControlManagerV1>,
    seat: Option<WlSeat>,
    payloads: HashMap<String, Vec<u8>>,
    done: bool,
}

impl RestoreState {
    fn new(payloads: Vec<(String, Vec<u8>)>) -> Self {
        Self {
            manager: None,
            seat: None,
            payloads: payloads.into_iter().collect(),
            done: false,
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for RestoreState {
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

impl Dispatch<ZwlrDataControlManagerV1, ()> for RestoreState {
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

impl Dispatch<WlSeat, ()> for RestoreState {
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

impl Dispatch<ZwlrDataControlDeviceV1, ()> for RestoreState {
    fn event_created_child(
        opcode: u16,
        qhandle: &QueueHandle<Self>,
    ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
        match opcode {
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
        if let zwlr_data_control_device_v1::Event::Finished = event {
            state.done = true;
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for RestoreState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlOfferV1,
        _: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for RestoreState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                debug!("send request for {mime_type}");
                let fd: OwnedFd = fd;
                if let Some(data) = state.payloads.get(&mime_type) {
                    let mut f = std::fs::File::from(fd);
                    if let Err(e) = f.write_all(data) {
                        warn!("write to fd for {mime_type}: {e}");
                    }
                } else {
                    warn!("no data stored for {mime_type}");
                    drop(fd);
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                debug!("source cancelled — restore complete");
                state.done = true;
            }
            _ => {}
        }
    }
}

pub fn restore_entry(payloads: Vec<(String, Vec<u8>)>) -> Result<()> {
    if payloads.is_empty() {
        return Ok(());
    }

    let conn = Connection::connect_to_env().context("connect to Wayland")?;
    let mut event_queue: EventQueue<RestoreState> = conn.new_event_queue();
    let qh = event_queue.handle();

    let display = conn.display();
    let _registry = display.get_registry(&qh, ());

    let mut state = RestoreState::new(payloads);
    event_queue.roundtrip(&mut state)?;

    let manager = state
        .manager
        .as_ref()
        .context("compositor lacks zwlr_data_control_manager_v1")?;
    let seat = state.seat.as_ref().context("no wl_seat")?;

    let device = manager.get_data_device(seat, &qh, ());
    let source = manager.create_data_source(&qh, ());

    for mime in state.payloads.keys() {
        source.offer(mime.clone());
    }

    device.set_selection(Some(&source));
    event_queue.flush()?;

    while !state.done {
        event_queue.blocking_dispatch(&mut state)?;
    }

    source.destroy();
    device.destroy();

    Ok(())
}
