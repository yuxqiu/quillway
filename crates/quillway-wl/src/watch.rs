//! When did the clipboard last change? A data-control device gets a
//! `selection` event on every copy; we note the time and never read the data.

use std::sync::atomic::{AtomicBool, Ordering};
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
#[derive(Clone)]
pub struct ClipboardWatch {
    last: Arc<Mutex<Option<Instant>>>,
    /// Cleared when the watch stops (connection lost, device finished).
    alive: Arc<AtomicBool>,
}

impl ClipboardWatch {
    /// Connect to the compositor and watch on a background thread.
    ///
    /// # Errors
    ///
    /// No Wayland connection, or the compositor lacks data-control.
    pub fn start() -> anyhow::Result<Self> {
        let conn = Connection::connect_to_env().context("connecting to Wayland")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let seat: WlSeat = globals.bind(&qh, 1..=1, ()).context("no wl_seat")?;
        let watch = Self { last: Arc::default(), alive: Arc::new(AtomicBool::new(true)) };
        let mut state = State { last: watch.last.clone(), alive: watch.alive.clone(), armed: false };

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
            while state.alive.load(Ordering::Relaxed) {
                if let Err(e) = queue.blocking_dispatch(&mut state) {
                    eprintln!("quillway: clipboard watch stopped: {e}");
                    state.alive.store(false, Ordering::Relaxed);
                }
            }
        })?;
        Ok(watch)
    }

    /// When the clipboard last changed after the watch started.
    #[must_use]
    pub fn last_change(&self) -> Option<Instant> {
        *self.last.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether the watch still sees copies; once it stops, it never restarts itself.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

struct State {
    last: Arc<Mutex<Option<Instant>>>,
    alive: Arc<AtomicBool>,
    armed: bool,
}

impl State {
    fn changed(&self) {
        if self.armed {
            *self.last.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
        }
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        device: &ExtDataControlDeviceV1,
        event: ext_device::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_device::Event::Selection { id: Some(offer) } => {
                state.changed();
                offer.destroy();
            }
            ext_device::Event::PrimarySelection { id: Some(offer) } => offer.destroy(),
            ext_device::Event::Finished => {
                device.destroy();
                state.alive.store(false, Ordering::Relaxed);
            }
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
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wlr_device::Event::Selection { id: Some(offer) } => {
                state.changed();
                offer.destroy();
            }
            wlr_device::Event::PrimarySelection { id: Some(offer) } => offer.destroy(),
            wlr_device::Event::Finished => {
                device.destroy();
                state.alive.store(false, Ordering::Relaxed);
            }
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
