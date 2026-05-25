use std::error::Error;
use std::ffi::c_void;
use std::io;
use std::os::fd::AsRawFd;
use std::ptr::NonNull;

use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1};

/// Wayland activation helper with input serial tracking.
pub struct WaylandActivation {
    connection: Connection,
    event_queue: EventQueue<ActivationState>,
    state: ActivationState,
    activation: xdg_activation_v1::XdgActivationV1,
    seat: wl_seat::WlSeat,
    surface: wl_surface::WlSurface,
    pending_token: Option<xdg_activation_token_v1::XdgActivationTokenV1>,
}

#[derive(Default)]
struct ActivationState {
    latest_serial: Option<u32>,
    token: Option<String>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
}

impl WaylandActivation {
    /// Create a Wayland activation helper from winit's raw Wayland handles.
    ///
    /// The helper attaches to winit's Wayland connection in guest mode. It owns only the protocol
    /// objects it creates, while the `wl_surface` proxy is borrowed from winit and only used as an
    /// activation-token argument.
    pub unsafe fn new(
        display: NonNull<c_void>,
        surface: NonNull<c_void>,
    ) -> Result<Self, Box<dyn Error>> {
        let backend = unsafe { Backend::from_foreign_display(display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);

        let surface_id = unsafe {
            ObjectId::from_ptr(wl_surface::WlSurface::interface(), surface.as_ptr().cast())?
        };
        let surface = wl_surface::WlSurface::from_id(&connection, surface_id)?;

        let (globals, mut event_queue) = registry_queue_init::<ActivationState>(&connection)?;
        let queue_handle = event_queue.handle();
        let activation = globals.bind(&queue_handle, 1..=1, ())?;
        let seat = globals.bind(&queue_handle, 1..=1, ())?;

        let mut state = ActivationState::default();
        event_queue.roundtrip(&mut state)?;

        Ok(Self { connection, event_queue, state, activation, seat, surface, pending_token: None })
    }

    /// Start requesting an activation token with the most recent Wayland input serial.
    pub fn request_token(&mut self) -> Result<bool, Box<dyn Error>> {
        self.dispatch_ready_events()?;

        if self.pending_token.is_some() {
            return Ok(false);
        }

        let Some(serial) = self.state.latest_serial else {
            return Ok(false);
        };

        self.state.token = None;

        let token = self.activation.get_activation_token(&self.event_queue.handle(), ());
        token.set_serial(serial, &self.seat);
        token.set_surface(&self.surface);
        token.commit();
        self.connection.flush()?;

        self.pending_token = Some(token);
        Ok(true)
    }

    /// Poll for a completed activation token request.
    pub fn poll_token(&mut self) -> Result<Option<String>, Box<dyn Error>> {
        self.dispatch_ready_events()?;

        if self.state.token.is_some() {
            self.pending_token = None;
        }

        Ok(self.state.token.take())
    }

    /// Cancel any pending activation-token request.
    pub fn cancel_token_request(&mut self) -> Result<(), Box<dyn Error>> {
        self.state.token = None;

        if let Some(token) = self.pending_token.take() {
            token.destroy();
            self.connection.flush()?;
        }

        Ok(())
    }

    fn dispatch_ready_events(&mut self) -> Result<(), Box<dyn Error>> {
        self.event_queue.dispatch_pending(&mut self.state)?;

        let Some(read_guard) = self.connection.prepare_read() else {
            self.event_queue.dispatch_pending(&mut self.state)?;
            return Ok(());
        };

        let fd = read_guard.connection_fd().as_raw_fd();
        let mut pollfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let ready = unsafe { libc::poll(&mut pollfd, 1, 0) };

        if ready < 0 {
            return Err(io::Error::last_os_error().into());
        } else if ready > 0 && pollfd.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0
        {
            read_guard.read()?;
        }

        self.event_queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ActivationState {
    fn event(
        _state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_activation_v1::XdgActivationV1, ()> for ActivationState {
    fn event(
        _state: &mut Self,
        _activation: &xdg_activation_v1::XdgActivationV1,
        _event: xdg_activation_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, ()> for ActivationState {
    fn event(
        state: &mut Self,
        token: &xdg_activation_token_v1::XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token: value } = event {
            state.token = Some(value);
            token.destroy();
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for ActivationState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _connection: &Connection,
        queue_handle: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities { capabilities: WEnum::Value(capabilities) } = event
        else {
            return;
        };

        if capabilities.contains(wl_seat::Capability::Pointer) {
            if state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(queue_handle, ()));
            }
        } else {
            state.pointer = None;
        }

        if capabilities.contains(wl_seat::Capability::Keyboard) {
            if state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(queue_handle, ()));
            }
        } else {
            state.keyboard = None;
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for ActivationState {
    fn event(
        state: &mut Self,
        _pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { serial, .. } | wl_pointer::Event::Button { serial, .. } => {
                state.latest_serial = Some(serial)
            },
            _ => (),
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for ActivationState {
    fn event(
        state: &mut Self,
        _keyboard: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { serial, .. } | wl_keyboard::Event::Key { serial, .. } => {
                state.latest_serial = Some(serial);
            },
            _ => (),
        }
    }
}
