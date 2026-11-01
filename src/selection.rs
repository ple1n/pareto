use std::{
    io::{Read, pipe},
    os::fd::{AsFd, BorrowedFd},
    sync::Mutex,
    thread,
};

use anyhow::{Context, Result, anyhow};
use futures::SinkExt;
use iced::Subscription;
use smithay_client_toolkit::reexports::{
    client::{
        Connection, Dispatch, Proxy, QueueHandle, event_created_child,
        globals::{BindError, GlobalList, GlobalListContents, registry_queue_init},
        protocol::{wl_registry, wl_seat::WlSeat},
    },
    protocols::ext::data_control::v1::client::{
        ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
        ext_data_control_manager_v1::ExtDataControlManagerV1,
        ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    },
    protocols::wp::primary_selection::zv1::client::{
        zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1,
        zwp_primary_selection_device_v1::{self, ZwpPrimarySelectionDeviceV1},
        zwp_primary_selection_offer_v1::{self, ZwpPrimarySelectionOfferV1},
    },
    protocols_wlr::data_control::v1::client::{
        zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
        zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
        zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    },
};

const TEXT_MIME_TYPE: &str = "text/plain;charset=utf-8";
const TEXT_MIME_FALLBACKS: [&str; 3] = [TEXT_MIME_TYPE, "text/plain", "UTF8_STRING"];

pub fn subscription() -> Subscription<String> {
    Subscription::run(|| {
        iced::stream::channel(10, |output| async move {
            thread::spawn(move || {
                if let Err(error) = watch(output) {
                    eprintln!("primary selection listener failed: {error:#}");
                }
            });
            futures::future::pending::<()>().await;
        })
    })
}

fn watch(output: futures::channel::mpsc::Sender<String>) -> Result<()> {
    let connection =
        Connection::connect_to_env().context("connecting to the Wayland compositor")?;
    let (globals, mut event_queue) =
        registry_queue_init(&connection).context("reading Wayland compositor globals")?;
    let queue_handle = event_queue.handle();
    let mut state = SelectionWatcher::new(&globals, &queue_handle, output)?;

    event_queue
        .roundtrip(&mut state)
        .context("initializing primary selection devices")?;

    loop {
        event_queue
            .blocking_dispatch(&mut state)
            .context("dispatching primary selection events")?;
    }
}

struct SelectionWatcher {
    output: futures::channel::mpsc::Sender<String>,
    _seat: WlSeat,
    _primary_manager: Option<ZwpPrimarySelectionDeviceManagerV1>,
    _primary_device: Option<ZwpPrimarySelectionDeviceV1>,
    _data_control_manager: Option<ZwlrDataControlManagerV1>,
    _data_control_device: Option<ZwlrDataControlDeviceV1>,
    _ext_data_control_manager: Option<ExtDataControlManagerV1>,
    _ext_data_control_device: Option<ExtDataControlDeviceV1>,
}

impl SelectionWatcher {
    fn new(
        globals: &GlobalList,
        queue_handle: &QueueHandle<Self>,
        output: futures::channel::mpsc::Sender<String>,
    ) -> Result<Self> {
        let seat = globals
            .bind::<WlSeat, _, _>(queue_handle, 1..=9, ())
            .context("Wayland compositor has no usable seat")?;
        let primary_manager =
            match globals.bind::<ZwpPrimarySelectionDeviceManagerV1, _, _>(queue_handle, 1..=1, ())
            {
                Ok(manager) => Some(manager),
                Err(BindError::NotPresent) => None,
                Err(error) => {
                    return Err(anyhow!(
                        "could not bind zwp_primary_selection manager: {error:?}"
                    ));
                }
            };
        let data_control_manager =
            match globals.bind::<ZwlrDataControlManagerV1, _, _>(queue_handle, 1..=2, ()) {
                Ok(manager) => Some(manager),
                Err(BindError::NotPresent) => None,
                Err(error) => {
                    return Err(anyhow!(
                        "could not bind zwlr_data_control manager: {error:?}"
                    ));
                }
            };
        let ext_data_control_manager =
            match globals.bind::<ExtDataControlManagerV1, _, _>(queue_handle, 1..=1, ()) {
                Ok(manager) => Some(manager),
                Err(BindError::NotPresent) => None,
                Err(error) => {
                    return Err(anyhow!(
                        "could not bind ext_data_control manager: {error:?}"
                    ));
                }
            };

        if primary_manager.is_none()
            && data_control_manager.is_none()
            && ext_data_control_manager.is_none()
        {
            return Err(anyhow!(
                "Wayland compositor exposes no supported primary-selection protocol"
            ));
        }

        let primary_device = primary_manager
            .as_ref()
            .map(|manager| manager.get_device(&seat, queue_handle, ()));
        let data_control_device = data_control_manager
            .as_ref()
            .map(|manager| manager.get_data_device(&seat, queue_handle, ()));
        let ext_data_control_device = ext_data_control_manager
            .as_ref()
            .map(|manager| manager.get_data_device(&seat, queue_handle, ()));

        Ok(Self {
            output,
            _seat: seat,
            _primary_manager: primary_manager,
            _primary_device: primary_device,
            _data_control_manager: data_control_manager,
            _data_control_device: data_control_device,
            _ext_data_control_manager: ext_data_control_manager,
            _ext_data_control_device: ext_data_control_device,
        })
    }

    fn receive_selection<O>(&self, offer: Option<O>, connection: &Connection)
    where
        O: SelectionOffer,
    {
        if let Some(offer) = offer {
            receive_offer(offer, connection, self.output.clone());
        }
    }
}

trait SelectionOffer {
    fn text_mime_type(&self) -> Option<String>;
    fn receive_text(&self, mime_type: String, fd: BorrowedFd<'_>);
    fn destroy_offer(&self);
}

#[derive(Default)]
struct OfferMimeTypes(Mutex<Vec<String>>);

impl OfferMimeTypes {
    fn preferred_text_type(&self) -> Option<String> {
        let offered = self.0.lock().unwrap();
        TEXT_MIME_FALLBACKS
            .iter()
            .find(|mime_type| {
                offered
                    .iter()
                    .any(|offered_type| offered_type == **mime_type)
            })
            .map(|mime_type| (*mime_type).to_owned())
    }
}

impl SelectionOffer for ZwpPrimarySelectionOfferV1 {
    fn text_mime_type(&self) -> Option<String> {
        self.data::<OfferMimeTypes>()?.preferred_text_type()
    }

    fn receive_text(&self, mime_type: String, fd: BorrowedFd<'_>) {
        self.receive(mime_type, fd);
    }

    fn destroy_offer(&self) {
        self.destroy();
    }
}

impl SelectionOffer for ZwlrDataControlOfferV1 {
    fn text_mime_type(&self) -> Option<String> {
        self.data::<OfferMimeTypes>()?.preferred_text_type()
    }

    fn receive_text(&self, mime_type: String, fd: BorrowedFd<'_>) {
        self.receive(mime_type, fd);
    }

    fn destroy_offer(&self) {
        self.destroy();
    }
}

impl SelectionOffer for ExtDataControlOfferV1 {
    fn text_mime_type(&self) -> Option<String> {
        self.data::<OfferMimeTypes>()?.preferred_text_type()
    }

    fn receive_text(&self, mime_type: String, fd: BorrowedFd<'_>) {
        self.receive(mime_type, fd);
    }

    fn destroy_offer(&self) {
        self.destroy();
    }
}

fn receive_offer<O>(
    offer: O,
    connection: &Connection,
    mut output: futures::channel::mpsc::Sender<String>,
) where
    O: SelectionOffer,
{
    let Some(mime_type) = offer.text_mime_type() else {
        return;
    };

    let (mut reader, writer) = match pipe() {
        Ok(pipe) => pipe,
        Err(error) => {
            eprintln!("could not create primary selection pipe: {error}");
            return;
        }
    };

    offer.receive_text(mime_type, writer.as_fd());
    offer.destroy_offer();
    drop(writer);

    if let Err(error) = connection.flush() {
        eprintln!("could not request primary selection data: {error}");
        return;
    }

    thread::spawn(move || {
        let mut text = String::new();
        if let Err(error) = reader.read_to_string(&mut text) {
            eprintln!("could not read primary selection data: {error}");
            return;
        }
        if futures::executor::block_on(output.send(text)).is_err() {
            eprintln!("primary selection event receiver was closed");
        }
    });
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for SelectionWatcher {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for SelectionWatcher {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: <WlSeat as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpPrimarySelectionDeviceManagerV1, ()> for SelectionWatcher {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpPrimarySelectionDeviceManagerV1,
        _event: <ZwpPrimarySelectionDeviceManagerV1 as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpPrimarySelectionDeviceV1, ()> for SelectionWatcher {
    fn event(
        state: &mut Self,
        _proxy: &ZwpPrimarySelectionDeviceV1,
        event: <ZwpPrimarySelectionDeviceV1 as Proxy>::Event,
        _data: &(),
        connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        if let zwp_primary_selection_device_v1::Event::Selection { id } = event {
            state.receive_selection(id, connection);
        }
    }

    event_created_child!(SelectionWatcher, ZwpPrimarySelectionDeviceV1, [
        zwp_primary_selection_device_v1::EVT_DATA_OFFER_OPCODE =>
            (ZwpPrimarySelectionOfferV1, OfferMimeTypes::default()),
    ]);
}

impl Dispatch<ZwpPrimarySelectionOfferV1, OfferMimeTypes> for SelectionWatcher {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpPrimarySelectionOfferV1,
        event: <ZwpPrimarySelectionOfferV1 as Proxy>::Event,
        data: &OfferMimeTypes,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        if let zwp_primary_selection_offer_v1::Event::Offer { mime_type } = event {
            data.0.lock().unwrap().push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for SelectionWatcher {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlManagerV1,
        _event: <ZwlrDataControlManagerV1 as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for SelectionWatcher {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlDeviceV1,
        event: <ZwlrDataControlDeviceV1 as Proxy>::Event,
        _data: &(),
        connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_device_v1::Event::PrimarySelection { id } = event {
            state.receive_selection(id, connection);
        }
    }

    event_created_child!(SelectionWatcher, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE =>
            (ZwlrDataControlOfferV1, OfferMimeTypes::default()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, OfferMimeTypes> for SelectionWatcher {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlOfferV1,
        event: <ZwlrDataControlOfferV1 as Proxy>::Event,
        data: &OfferMimeTypes,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            data.0.lock().unwrap().push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for SelectionWatcher {
    fn event(
        _state: &mut Self,
        _proxy: &ExtDataControlManagerV1,
        _event: <ExtDataControlManagerV1 as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for SelectionWatcher {
    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlDeviceV1,
        event: <ExtDataControlDeviceV1 as Proxy>::Event,
        _data: &(),
        connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        if let ext_data_control_device_v1::Event::PrimarySelection { id } = event {
            state.receive_selection(id, connection);
        }
    }

    event_created_child!(SelectionWatcher, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE =>
            (ExtDataControlOfferV1, OfferMimeTypes::default()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, OfferMimeTypes> for SelectionWatcher {
    fn event(
        _state: &mut Self,
        _proxy: &ExtDataControlOfferV1,
        event: <ExtDataControlOfferV1 as Proxy>::Event,
        data: &OfferMimeTypes,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            data.0.lock().unwrap().push(mime_type);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{OfferMimeTypes, TEXT_MIME_TYPE};

    #[test]
    fn prefers_utf8_plain_text_and_falls_back_to_supported_text_types() {
        let offered = OfferMimeTypes(Default::default());
        offered.0.lock().unwrap().extend([
            "text/html".to_owned(),
            "text/plain".to_owned(),
            TEXT_MIME_TYPE.to_owned(),
        ]);
        assert_eq!(
            offered.preferred_text_type().as_deref(),
            Some(TEXT_MIME_TYPE)
        );

        let offered = OfferMimeTypes(Default::default());
        offered.0.lock().unwrap().push("text/plain".to_owned());
        assert_eq!(offered.preferred_text_type().as_deref(), Some("text/plain"));
    }

    #[test]
    fn rejects_offers_without_a_supported_text_type() {
        let offered = OfferMimeTypes(Default::default());
        offered.0.lock().unwrap().push("text/html".to_owned());
        assert_eq!(offered.preferred_text_type(), None);
    }
}
