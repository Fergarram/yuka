//
// Wayland server state + protocol handlers
//
// Implements the minimum Smithay frontend for the first pass: wl_compositor,
// wl_shm, xdg_shell and wp_viewporter. dmabuf and single_pixel are added with
// the EGL import milestone. raylib drives the frame loop, so we use a raw
// ListeningSocket and pump dispatch_clients / flush_clients once per frame
// instead of calloop.
//

use std::os::fd::AsRawFd;
use std::sync::Arc;

use smithay::backend::allocator::{Buffer, Format, Fourcc, Modifier};
use smithay::reexports::wayland_server::{
    backend::{ClientData, ClientId, DisconnectReason, ObjectId},
    protocol::{wl_buffer::WlBuffer, wl_seat::WlSeat, wl_surface::WlSurface},
    Client, Display, ListeningSocket, Resource,
};
use smithay::desktop::{PopupKind, PopupManager};
use smithay::input::keyboard::{KeyboardHandle, XkbConfig};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::input::pointer::{CursorImageStatus, PointerHandle};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::utils::Serial;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    with_states, BufferAssignment, CompositorClientState, CompositorHandler,
    CompositorState, SurfaceAttributes,
};
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionHandler, PrimarySelectionState,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shm::with_buffer_contents;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface,
    XdgShellHandler, XdgShellState,
};
use smithay::wayland::dmabuf::{
    get_dmabuf, DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState,
    ImportNotifier,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::viewporter::ViewporterState;
use smithay::{
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_output,
    delegate_primary_selection, delegate_seat, delegate_shm, delegate_viewporter,
    delegate_xdg_shell,
};

use smithay::backend::allocator::dmabuf::Dmabuf;

use crate::egl::{DmabufInfo, DmabufPlane};

//
// Types
//

pub struct State {
    pub compositor_state: CompositorState,
    pub shm_state: ShmState,
    pub xdg_state: XdgShellState,
    // Held for the program's lifetime to keep the globals registered.
    #[allow(dead_code)]
    pub viewporter_state: ViewporterState,
    pub seat_state: SeatState<State>,
    // The wl_output clients size themselves against, plus its global and the
    // xdg_output manager. Held to keep them registered.
    #[allow(dead_code)]
    pub output: Output,
    #[allow(dead_code)]
    pub output_global: GlobalId,
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    // Held to keep the wl_seat global alive; used for the keyboard next.
    #[allow(dead_code)]
    pub seat: Seat<State>,
    pub pointer: PointerHandle<State>,
    pub keyboard: KeyboardHandle<State>,
    pub dmabuf_state: DmabufState,
    #[allow(dead_code)]
    pub dmabuf_global: DmabufGlobal,
    // Clipboard, drag and drop, and the middle-click (primary) selection. Real
    // toolkits expect these to exist: GTK builds an incomplete GdkSeat without a
    // data device, which is what Chromium's "gdk_seat_get_keyboard" assertion is,
    // and no client can copy or paste without it.
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    // Popup tracking (menus). Smithay owns the tree, the positioner maths and
    // the parent relationships; we only ask it where they go.
    pub popups: PopupManager,

    // Surfaces that committed since the last drain. The render side reads and
    // clears this each frame. Plain surface handles, no rendering state here.
    pub committed: Vec<WlSurface>,
    // wl_buffers destroyed by clients; the render side evicts their cached
    // EGLImages. Drained each frame.
    pub dead_dmabufs: Vec<ObjectId>,
    // The dmabuf we are currently displaying per surface, kept alive (not
    // released) so the client cannot overwrite it while we re-sample it each
    // frame. Released only when a newer buffer replaces it. shm buffers are not
    // held (we copy them out, so they are released immediately on upload).
    pub held_dmabufs: Vec<(WlSurface, WlBuffer)>,
}

pub struct Server {
    pub display: Display<State>,
    pub listener: ListeningSocket,
    pub clients: Vec<Client>,
    pub socket_name: String,
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

//
// Client data
//

impl ClientData for ClientState {
    fn initialized(&self, _id: ClientId) {}
    fn disconnected(&self, _id: ClientId, _reason: DisconnectReason) {}
}

//
// Init
//

pub fn init(
    dmabuf_formats: Vec<(u32, u64)>,
    render_node_dev: Option<u64>,
    screen_w: i32,
    screen_h: i32,
) -> Result<(Server, State), Box<dyn std::error::Error>> {
    let display: Display<State> = Display::new()?;
    let dh = display.handle();

    let compositor_state = CompositorState::new::<State>(&dh);
    let shm_state = ShmState::new::<State>(&dh, vec![]);
    let xdg_state = XdgShellState::new::<State>(&dh);
    let viewporter_state = ViewporterState::new::<State>(&dh);
    let data_device_state = DataDeviceState::new::<State>(&dh);
    let primary_selection_state = PrimarySelectionState::new::<State>(&dh);

    // A wl_output, and xdg_output alongside it. Toolkits that derive their scale
    // and window sizing from an output (GTK, Chromium) never map a window without
    // one, so its absence looks exactly like a client that silently does nothing.
    // One output the size of the screen, even though the canvas is unbounded: it
    // is what clients size themselves against.
    let output = Output::new(
        "om_wm-0".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "om_wm".to_string(),
            model: "canvas".to_string(),
        },
    );
    let mode = Mode { size: (screen_w, screen_h).into(), refresh: 60_000 };
    output.change_current_state(Some(mode), None, None, Some((0, 0).into()));
    output.set_preferred(mode);
    let output_global = output.create_global::<State>(&dh);
    let output_manager_state = OutputManagerState::new_with_xdg_output::<State>(&dh);
    let mut seat_state = SeatState::new();
    // wl_seat with a pointer so clients can receive pointer events.
    let mut seat = seat_state.new_wl_seat(&dh, "seat0");
    let pointer = seat.add_pointer();
    let keyboard = seat
        .add_keyboard(XkbConfig::default(), 600, 25)
        .expect("keyboard");

    // Advertise the dmabuf formats EGL reported as importable.
    let formats: Vec<Format> = dmabuf_formats
        .into_iter()
        .filter_map(|(code, modifier)| {
            Fourcc::try_from(code).ok().map(|code| Format {
                code,
                modifier: Modifier::from(modifier),
            })
        })
        .collect();
    println!("om_wm: advertising {} dmabuf formats", formats.len());

    let mut dmabuf_state = DmabufState::new();
    // Prefer a v4 global with default feedback (advertises the render node),
    // which Mesa needs to actually allocate dmabufs. Fall back to a v3 global.
    let dmabuf_global = match render_node_dev {
        Some(dev) => {
            let feedback = DmabufFeedbackBuilder::new(dev as libc::dev_t, formats)
                .build()
                .expect("dmabuf feedback build");
            dmabuf_state
                .create_global_with_default_feedback::<State>(&dh, &feedback)
        }
        None => dmabuf_state.create_global::<State>(&dh, formats),
    };

    let listener = ListeningSocket::bind_auto("wayland", 1..32)?;
    let socket_name = listener
        .socket_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();

    let state = State {
        compositor_state,
        shm_state,
        xdg_state,
        viewporter_state,
        seat_state,
        output,
        output_global,
        output_manager_state,
        seat,
        pointer,
        keyboard,
        dmabuf_state,
        dmabuf_global,
        data_device_state,
        primary_selection_state,
        popups: PopupManager::default(),
        committed: Vec::new(),
        dead_dmabufs: Vec::new(),
        held_dmabufs: Vec::new(),
    };

    let server = Server {
        display,
        listener,
        clients: Vec::new(),
        socket_name,
    };

    Ok((server, state))
}

//
// Per frame pump
//

pub fn accept_and_dispatch(server: &mut Server, state: &mut State) {
    if let Ok(Some(stream)) = server.listener.accept() {
        match server
            .display
            .handle()
            .insert_client(stream, Arc::new(ClientState::default()))
        {
            Ok(client) => server.clients.push(client),
            Err(e) => eprintln!("om_wm: insert_client failed: {e}"),
        }
    }

    server
        .display
        .dispatch_clients(state)
        .expect("dispatch_clients");
}

pub fn flush(server: &mut Server) {
    server.display.flush_clients().expect("flush_clients");
}

//
// Surface access (bridge to the render zone)
//

// Current toplevel root surfaces, cloned handles for the render side.
pub fn toplevel_surfaces(state: &State) -> Vec<WlSurface> {
    state
        .xdg_state
        .toplevel_surfaces()
        .iter()
        .map(|t| t.wl_surface().clone())
        .collect()
}

// True if the surface is a real xdg toplevel window, not a cursor or subsurface
// surface (which also commit buffers but must never be composited as a quad).
pub fn is_toplevel(state: &State, surface: &WlSurface) -> bool {
    state
        .xdg_state
        .toplevel_surfaces()
        .iter()
        .any(|t| t.wl_surface() == surface)
}

// True if the surface is a mapped xdg popup (a menu), which we do composite, but
// positioned by its parent rather than placed on the canvas.
pub fn is_popup(state: &State, surface: &WlSurface) -> bool {
    state.popups.find_popup(surface).is_some()
}

// The popups of a toplevel, each with the offset of its surface's top-left from
// that toplevel's surface top-left. Smithay resolves the positioner and the
// nesting; the two geometry terms convert between window geometry and surface
// coordinates, which differ by however much each client pads for shadows:
//
//   surface offset = parent geometry loc + popup offset - popup geometry loc
//
// Without them a menu lands wherever the client's shadow margins happen to put
// it rather than at the pointer.
pub fn popups_of(root: &WlSurface) -> Vec<(WlSurface, f32, f32)> {
    let (rx, ry) = geometry_loc(root);
    PopupManager::popups_for_surface(root)
        .map(|(popup, offset)| {
            let g = popup.geometry().loc;
            (
                popup.wl_surface().clone(),
                rx + (offset.x - g.x) as f32,
                ry + (offset.y - g.y) as f32,
            )
        })
        .collect()
}

// Where a surface's window geometry starts relative to the surface itself, as set
// by xdg_surface::set_window_geometry. Non-zero for clients that draw shadows.
fn geometry_loc(surface: &WlSurface) -> (f32, f32) {
    with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceCachedState>()
            .current()
            .geometry
            .map(|g| (g.loc.x as f32, g.loc.y as f32))
            .unwrap_or((0.0, 0.0))
    })
}

// Tell every open popup it is done, which is how a menu closes when the click
// lands somewhere else. Clients destroy the surface in response.
pub fn dismiss_popups(state: &State) {
    for root in toplevel_surfaces(state) {
        for (popup, _) in PopupManager::popups_for_surface(&root) {
            match popup {
                PopupKind::Xdg(p) => p.send_popup_done(),
                _ => {}
            }
        }
    }
}

// Whether any popup is open at all.
pub fn any_popup(state: &State) -> bool {
    toplevel_surfaces(state)
        .iter()
        .any(|root| PopupManager::popups_for_surface(root).next().is_some())
}

// If the surface has a newly committed shm buffer, invoke f with its
// dimensions, stride and a pointer to the pixel data (valid only for the call),
// then release the buffer and clear the pending assignment. Returns true when a
// shm buffer was handled.
pub fn take_shm_buffer<F>(surface: &WlSurface, mut f: F) -> bool
where
    F: FnMut(i32, i32, i32, *const u8),
{
    with_states(surface, |data| {
        let mut guard = data.cached_state.get::<SurfaceAttributes>();
        let attrs = guard.current();

        let buffer = match &attrs.buffer {
            Some(BufferAssignment::NewBuffer(b)) => b.clone(),
            Some(BufferAssignment::Removed) => {
                attrs.buffer = None;
                return false;
            }
            None => return false,
        };

        let handled = with_buffer_contents(&buffer, |ptr, _len, spec| {
            let base = unsafe { ptr.offset(spec.offset as isize) };
            f(spec.width, spec.height, spec.stride, base);
        })
        .is_ok();

        if handled {
            buffer.release();
            attrs.buffer = None;
        }
        handled
    })
}

// If the surface has a newly committed dmabuf, invoke f with its plane layout,
// then release the buffer and clear the pending assignment. Returns true when a
// dmabuf was handled. eglCreateImageKHR dups the plane fds, so releasing right
// after the call is safe.
pub fn take_dmabuf_and_retain<F>(
    state: &mut State,
    surface: &WlSurface,
    mut f: F,
) -> bool
where
    F: FnMut(ObjectId, &DmabufInfo),
{
    let mut new_buffer: Option<WlBuffer> = None;

    let handled = with_states(surface, |data| {
        let mut guard = data.cached_state.get::<SurfaceAttributes>();
        let attrs = guard.current();

        let buffer = match &attrs.buffer {
            Some(BufferAssignment::NewBuffer(b)) => b.clone(),
            _ => return false,
        };

        let dmabuf = match get_dmabuf(&buffer) {
            Ok(d) => d,
            Err(_) => return false,
        };

        let key = buffer.id();

        let fds: Vec<i32> = dmabuf.handles().map(|h| h.as_raw_fd()).collect();
        let offsets: Vec<u32> = dmabuf.offsets().collect();
        let strides: Vec<u32> = dmabuf.strides().collect();
        let planes: Vec<DmabufPlane> = (0..dmabuf.num_planes())
            .map(|i| DmabufPlane {
                fd: fds[i],
                offset: offsets[i],
                stride: strides[i],
            })
            .collect();

        let fmt = dmabuf.format();
        let info = DmabufInfo {
            width: dmabuf.width() as i32,
            height: dmabuf.height() as i32,
            fourcc: fmt.code as u32,
            modifier: u64::from(fmt.modifier),
            has_modifier: dmabuf.has_modifier(),
            planes,
        };

        f(key, &info);

        // Retain instead of release: we keep sampling this buffer every frame
        // it is displayed, so it must not be released until a newer buffer
        // replaces it (see retain_replace).
        new_buffer = Some(buffer);
        attrs.buffer = None;
        true
    });

    if let Some(buf) = new_buffer {
        retain_replace(&mut state.held_dmabufs, surface, buf);
    }
    handled
}

// Hold new_buf as the surface's displayed buffer, releasing the previous one it
// replaces. A same-id recommit keeps the existing hold and drops the extra clone.
fn retain_replace(
    held: &mut Vec<(WlSurface, WlBuffer)>,
    surface: &WlSurface,
    new_buf: WlBuffer,
) {
    if let Some(slot) = held.iter_mut().find(|(s, _)| s == surface) {
        if slot.1.id() != new_buf.id() {
            slot.1.release();
            slot.1 = new_buf;
        }
    } else {
        held.push((surface.clone(), new_buf));
    }
}

// Release and drop any dmabuf held for this surface (e.g. it switched to shm).
pub fn release_held_dmabuf(state: &mut State, surface: &WlSurface) {
    if let Some(pos) = state.held_dmabufs.iter().position(|(s, _)| s == surface) {
        state.held_dmabufs[pos].1.release();
        state.held_dmabufs.swap_remove(pos);
    }
}

// Drop held buffers whose surface has died (client gone); release is a no-op then.
pub fn prune_held(state: &mut State) {
    state.held_dmabufs.retain(|(s, _)| s.is_alive());
}

// Signal the surface it may render its next frame.
pub fn send_frame_callbacks(surface: &WlSurface, time_ms: u32) {
    with_states(surface, |data| {
        let mut guard = data.cached_state.get::<SurfaceAttributes>();
        let attrs = guard.current();
        for cb in attrs.frame_callbacks.drain(..) {
            cb.done(time_ms);
        }
    });
}

//
// Handlers
//

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a Client,
    ) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        // A popup counts as mapped from its first commit, which is what moves it
        // out of Smithay's unmapped list and into its parent's tree.
        self.popups.commit(surface);
        if !self.committed.iter().any(|s| s == surface) {
            self.committed.push(surface.clone());
        }
    }
}

// Nothing to do on bind: one static output, no per-client state.
impl OutputHandler for State {}

// Selections are handled entirely by Smithay: it keeps the current source per
// seat and hands offers to clients. We attach no data of our own and take the
// default drag-and-drop action negotiation.
impl SelectionHandler for State {
    type SelectionUserData = ();
}

impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, buffer: &WlBuffer) {
        self.dead_dmabufs.push(buffer.id());
    }
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {}
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        // We import lazily at commit time; optimistically accept here.
        let _ = notifier.successful::<State>();
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        surface.send_configure();
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        // The positioner already carries where the client wants this, relative to
        // the parent's geometry, and we do not constrain it to an output: the
        // canvas has no edges to be pushed away from.
        let _ = surface.send_configure();
        if let Err(e) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            eprintln!("om_wm: popup track failed: {e}");
        }
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        _positioner: PositionerState,
        token: u32,
    ) {
        surface.send_repositioned(token);
    }
}

//
// Delegates
//

delegate_compositor!(State);
delegate_shm!(State);
delegate_xdg_shell!(State);
delegate_viewporter!(State);
delegate_dmabuf!(State);
delegate_seat!(State);
delegate_output!(State);
delegate_data_device!(State);
delegate_primary_selection!(State);
