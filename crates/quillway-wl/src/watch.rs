//! When did the clipboard last change? A data-control device gets a
//! `selection` event on every copy; we note the time and never read the data.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Context;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self as ext_device, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::ExtDataControlOfferV1,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self as wlr_device, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
};

/// Shared handle to the time of the last clipboard change.
#[derive(Clone, Default)]
pub struct ClipboardWatch(Arc<Mutex<Option<Instant>>>);

impl ClipboardWatch {
    /// Connect to the compositor and watch on a background thread.
    pub fn start() -> anyhow::Result<Self> {
        let conn = Connection::connect_to_env().context("connecting to Wayland")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let seat: WlSeat = globals.bind(&qh, 1..=1, ()).context("no wl_seat")?;
        let watch = Self::default();
        let mut state = State { last: watch.0.clone(), armed: false };

        if let Ok(m) = globals.bind::<ExtDataControlManagerV1, _, _>(&qh, 1..=1, ()) {
            m.get_data_device(&seat, &qh, ());
        } else {
            let m: ZwlrDataControlManagerV1 =
                globals.bind(&qh, 1..=2, ()).context("the compositor supports neither ext- nor wlr-data-control")?;
            m.get_data_device(&seat, &qh, ());
        }
        // A new device reports the current selection at once; that isn't a copy.
        queue.roundtrip(&mut state)?;
        state.armed = true;

        std::thread::Builder::new().name("clipboard-watch".into()).spawn(move || {
            loop {
                if let Err(e) = queue.blocking_dispatch(&mut state) {
                    eprintln!("quillway: clipboard watch stopped: {e}");
                    return;
                }
            }
        })?;
        Ok(watch)
    }

    pub fn last_change(&self) -> Option<Instant> {
        *self.0.lock().expect("watch lock")
    }
}

struct State {
    last: Arc<Mutex<Option<Instant>>>,
    armed: bool,
}

impl State {
    fn changed(&self) {
        if self.armed {
            *self.last.lock().expect("watch lock") = Some(Instant::now());
        }
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        device: &ExtDataControlDeviceV1,
        event: ext_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_device::Event::Selection { id: Some(offer) } => {
                state.changed();
                offer.destroy();
            }
            ext_device::Event::PrimarySelection { id: Some(offer) } => offer.destroy(),
            ext_device::Event::Finished => device.destroy(),
            _ => {}
        }
    }

    event_created_child!(State, ExtDataControlDeviceV1, [
        ext_device::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        device: &ZwlrDataControlDeviceV1,
        event: wlr_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wlr_device::Event::Selection { id: Some(offer) } => {
                state.changed();
                offer.destroy();
            }
            wlr_device::Event::PrimarySelection { id: Some(offer) } => offer.destroy(),
            wlr_device::Event::Finished => device.destroy(),
            _ => {}
        }
    }

    event_created_child!(State, ZwlrDataControlDeviceV1, [
        wlr_device::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

/// Objects whose events we don't need.
macro_rules! ignore_events {
    ($($ty:ty),*) => {$(
        impl Dispatch<$ty, ()> for State {
            fn event(_: &mut Self, _: &$ty, _: <$ty as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}

ignore_events!(
    WlSeat,
    ExtDataControlManagerV1,
    ExtDataControlOfferV1,
    ZwlrDataControlManagerV1,
    ZwlrDataControlOfferV1
);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
