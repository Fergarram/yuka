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
use smithay::input::pointer::CursorImageStatus;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::utils::Serial;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    with_states, BufferAssignment, CompositorClientState, CompositorHandler,
    CompositorState, SurfaceAttributes,
};
use smithay::wayland::shm::with_buffer_contents;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler,
    XdgShellState,
};
use smithay::wayland::dmabuf::{
    get_dmabuf, DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState,
    ImportNotifier,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::viewporter::ViewporterState;
use smithay::{
    delegate_compositor, delegate_dmabuf, delegate_shm, delegate_viewporter,
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
    pub dmabuf_state: DmabufState,
    #[allow(dead_code)]
    pub dmabuf_global: DmabufGlobal,

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
) -> Result<(Server, State), Box<dyn std::error::Error>> {
    let display: Display<State> = Display::new()?;
    let dh = display.handle();

    let compositor_state = CompositorState::new::<State>(&dh);
    let shm_state = ShmState::new::<State>(&dh, vec![]);
    let xdg_state = XdgShellState::new::<State>(&dh);
    let viewporter_state = ViewporterState::new::<State>(&dh);
    let seat_state = SeatState::new();

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
        dmabuf_state,
        dmabuf_global,
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
        if !self.committed.iter().any(|s| s == surface) {
            self.committed.push(surface.clone());
        }
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
        let _ = surface.send_configure();
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
