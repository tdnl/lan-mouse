use crate::error::EmulationError;

use super::{Emulation, error::WlrootsEmulationCreationError};
use async_trait::async_trait;
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::time::{SystemTime, UNIX_EPOCH};
use wayland_client::WEnum;
use wayland_client::backend::WaylandError;
use xkbcommon::xkb;

use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1 as VpManager,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1 as Vp,
};

use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1 as VkManager,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1 as Vk,
};

use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_seat},
};

use input_event::{Event, KeyboardEvent, PointerEvent};

use super::EmulationHandle;
use super::error::WaylandBindError;

// xkbcommon objects carry no thread affinity and are only used from the emulation task
struct XkbKeymap(xkb::Keymap);
unsafe impl Send for XkbKeymap {}

struct XkbState(xkb::State);
unsafe impl Send for XkbState {}

const XKB_KEYCODE_OFFSET: u32 = 8;

struct State {
    keymap: Option<(u32, OwnedFd, u32)>,
    xkb_keymap: Option<XkbKeymap>,
    /// shared by all clients, so the keymap fd is sent once instead of once per client
    keyboard: Option<Vk>,
    xkb_state: Option<XkbState>,
    input_for_client: HashMap<EmulationHandle, VirtualInput>,
    seat: wl_seat::WlSeat,
    qh: QueueHandle<Self>,
    vpm: VpManager,
    vkm: VkManager,
}

// App State, implements Dispatch event handlers
pub(crate) struct WlrootsEmulation {
    last_flush_failed: bool,
    state: State,
    queue: EventQueue<State>,
}

impl WlrootsEmulation {
    pub(crate) fn new() -> Result<Self, WlrootsEmulationCreationError> {
        let conn = Connection::connect_to_env()?;
        let (globals, queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();

        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 7..=8, ())
            .map_err(|e| WaylandBindError::new(e, "wl_seat 7..=8"))?;

        let vpm: VpManager = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "wlr-virtual-pointer-unstable-v1"))?;
        let vkm: VkManager = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "virtual-keyboard-unstable-v1"))?;

        let input_for_client: HashMap<EmulationHandle, VirtualInput> = HashMap::new();

        let mut emulate = WlrootsEmulation {
            last_flush_failed: false,
            state: State {
                keymap: None,
                xkb_keymap: None,
                keyboard: None,
                xkb_state: None,
                input_for_client,
                seat,
                vpm,
                vkm,
                qh,
            },
            queue,
        };
        while emulate.state.keymap.is_none() {
            emulate.queue.blocking_dispatch(&mut emulate.state)?;
        }
        emulate.state.xkb_keymap = emulate
            .state
            .keymap
            .as_ref()
            .and_then(|(format, fd, size)| compile_keymap(*format, fd, *size));
        if emulate.state.xkb_keymap.is_none() {
            log::warn!("failed to compile the seat keymap: modifier state will not be tracked");
        }
        Ok(emulate)
    }
}

/// The seat keymap is the one uploaded to the virtual keyboard, so key events
/// fed through it yield the modifier state the compositor expects.
fn compile_keymap(format: u32, fd: &OwnedFd, size: u32) -> Option<XkbKeymap> {
    if format != u32::from(wl_keyboard::KeymapFormat::XkbV1) {
        log::warn!("unsupported keymap format: {format}");
        return None;
    }
    let file = File::from(fd.try_clone().ok()?);
    let mut buffer = vec![0u8; size as usize];
    file.read_exact_at(&mut buffer, 0).ok()?;
    let end = buffer
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(buffer.len());
    let text = String::from_utf8_lossy(&buffer[..end]).into_owned();
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    xkb::Keymap::new_from_string(
        &context,
        text,
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .map(XkbKeymap)
}

impl State {
    fn add_client(&mut self, client: EmulationHandle) {
        let pointer: Vp = self.vpm.create_virtual_pointer(None, &self.qh, ());

        let keyboard = match self.keyboard.as_ref() {
            Some(keyboard) => keyboard.clone(),
            None => {
                let keyboard: Vk = self.vkm.create_virtual_keyboard(&self.seat, &self.qh, ());
                // TODO: use server side keymap
                let Some((format, fd, size)) = self.keymap.as_ref() else {
                    panic!("no keymap");
                };
                keyboard.keymap(*format, fd.as_fd(), *size);
                self.keyboard = Some(keyboard.clone());
                self.xkb_state = self
                    .xkb_keymap
                    .as_ref()
                    .map(|keymap| XkbState(xkb::State::new(&keymap.0)));
                keyboard
            }
        };

        let vinput = VirtualInput { pointer, keyboard };

        self.input_for_client.insert(client, vinput);
    }

    fn destroy_client(&mut self, handle: EmulationHandle) {
        // the shared keyboard outlives every client; keys are released by InputEmulation::destroy
        if let Some(input) = self.input_for_client.remove(&handle) {
            input.pointer.destroy();
        }
    }
}

#[async_trait]
impl Emulation for WlrootsEmulation {
    async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        if let Some(virtual_input) = self.state.input_for_client.get(&handle) {
            if self.last_flush_failed {
                match self.queue.flush() {
                    Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                        /*
                         * outgoing buffer is full - sending more events
                         * will overwhelm the output buffer and leave the
                         * wayland connection in a broken state
                         */
                        log::warn!("can't keep up, discarding event: ({handle}) - {event:?}");
                        return Ok(());
                    }
                    _ => {}
                }
            }
            virtual_input
                .consume_event(event, self.state.xkb_state.as_mut())
                .unwrap_or_else(|_| panic!("failed to convert event: {event:?}"));
            match self.queue.flush() {
                Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.last_flush_failed = true;
                    log::warn!("can't keep up, discarding event: ({handle}) - {event:?}");
                }
                Err(WaylandError::Protocol(e)) => panic!("wayland protocol violation: {e}"),
                Ok(()) => self.last_flush_failed = false,
                Err(e) => Err(e)?,
            }
        }
        Ok(())
    }

    async fn create(&mut self, handle: EmulationHandle) {
        self.state.add_client(handle);
        if let Err(e) = self.queue.flush() {
            log::error!("{e}");
        }
    }
    async fn destroy(&mut self, handle: EmulationHandle) {
        self.state.destroy_client(handle);
        if let Err(e) = self.queue.flush() {
            log::error!("{e}");
        }
    }
    async fn terminate(&mut self) {
        /* nothing to do */
    }
}

struct VirtualInput {
    pointer: Vp,
    keyboard: Vk,
}

impl VirtualInput {
    fn consume_event(&self, event: Event, xkb_state: Option<&mut XkbState>) -> Result<(), ()> {
        // the peer's timestamps use its own clock and unit (libei sends microseconds,
        // windows and macos send 0); clients compare them to detect double clicks
        let now: u32 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u32;

        match event {
            Event::Pointer(e) => {
                match e {
                    PointerEvent::Motion { dx, dy, .. } => self.pointer.motion(now, dx, dy),
                    PointerEvent::Button { button, state, .. } => {
                        let state: ButtonState = state.try_into()?;
                        self.pointer.button(now, button, state);
                    }
                    PointerEvent::Axis { axis, value, .. } => {
                        let axis: Axis = (axis as u32).try_into()?;
                        self.pointer.axis(now, axis, value);
                        self.pointer.axis_source(AxisSource::Continuous);
                        self.pointer.frame();
                    }
                    PointerEvent::AxisDiscrete120 { axis, value } => {
                        let axis: Axis = (axis as u32).try_into()?;
                        self.pointer
                            .axis_discrete(now, axis, value as f64 / 8., value / 120);
                        self.pointer.axis_source(AxisSource::Wheel);
                        self.pointer.frame();
                    }
                }
                self.pointer.frame();
            }
            Event::Keyboard(e) => match e {
                KeyboardEvent::Key { key, state, .. } => {
                    self.keyboard.key(now, key, state as u32);
                    // compositors apply this mask as is, they do not derive it from key events
                    if let Some(XkbState(xkb_state)) = xkb_state {
                        let direction = match state {
                            1 => xkb::KeyDirection::Down,
                            _ => xkb::KeyDirection::Up,
                        };
                        let keycode = xkb::Keycode::new(key + XKB_KEYCODE_OFFSET);
                        let changed = xkb_state.update_key(keycode, direction);
                        if changed != 0 {
                            log::trace!("key {key} changed xkb state components {changed:#x}");
                            self.keyboard.modifiers(
                                xkb_state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
                                xkb_state.serialize_mods(xkb::STATE_MODS_LATCHED),
                                xkb_state.serialize_mods(xkb::STATE_MODS_LOCKED),
                                xkb_state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
                            );
                        }
                    }
                }
                KeyboardEvent::Modifiers {
                    depressed,
                    latched,
                    locked,
                    group,
                } => {
                    // Synchronize internal modifier state, assuming server is authoritative
                    if let Some(XkbState(xkb_state)) = xkb_state {
                        xkb_state.update_mask(depressed, latched, locked, 0, 0, group);
                    }
                    self.keyboard.modifiers(depressed, latched, locked, group);
                }
            },
        }
        Ok(())
    }
}

delegate_noop!(State: Vp);
delegate_noop!(State: Vk);
delegate_noop!(State: VpManager);
delegate_noop!(State: VkManager);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut State,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: <WlKeyboard as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event {
            state.keymap = Some((u32::from(format), fd, size));
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        seat: &WlSeat,
        event: <WlSeat as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qhandle, ());
            }
        }
    }
}
