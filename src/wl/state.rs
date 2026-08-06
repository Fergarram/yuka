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
    Client, Display, DisplayHandle, ListeningSocket, Resource,
};
use smithay::desktop::utils::under_from_surface_tree;
use smithay::desktop::{
    find_popup_root_surface, PopupKeyboardGrab, PopupKind, PopupManager,
    PopupPointerGrab, PopupUngrabStrategy, WindowSurfaceType,
};
use smithay::input::pointer::Focus;
use smithay::input::keyboard::{KeyboardHandle, XkbConfig};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::input::pointer::{CursorImageStatus, CursorImageSurfaceData, PointerHandle};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::utils::{Logical, Point, Serial};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    get_parent, get_role, with_states, with_surface_tree_downward, BufferAssignment,
    CompositorClientState, CompositorHandler, CompositorState, SubsurfaceCachedState,
    SurfaceAttributes, TraversalAction,
};
use smithay::wayland::selection::data_device::{
    set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState,
    ServerDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shm::with_buffer_contents;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::{
    ResizeEdge, State as ToplevelState,
};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface,
    XdgShellHandler, XdgShellState,
};
use smithay::wayland::dmabuf::{
    get_dmabuf, DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState,
    ImportNotifier,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::pointer_gestures::PointerGesturesState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};
use smithay::{
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_output,
    delegate_pointer_gestures, delegate_seat, delegate_shm, delegate_viewporter,
    delegate_xdg_activation, delegate_xdg_shell,
};

use smithay::backend::allocator::dmabuf::Dmabuf;

use crate::egl::{DmabufInfo, DmabufPlane};

//
// Constants
//

// How old an activation token may be and still be honoured. It is the gap between a user
// clicking something and the application it launched finishing its own startup, so it has to
// be generous: a browser starting cold is seconds, not milliseconds. Bounded all the same, so
// a token cannot be pocketed and spent much later.
const ACTIVATION_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(30);

//
// Types
//

pub struct State {
    // Kept because the selection has to be told who holds the keyboard, and that is a
    // freestanding call rather than something the seat does for itself. init built one and
    // dropped it, which is why nothing could paste.
    pub display_handle: DisplayHandle,
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
    // The seat itself, which the clipboard is a property of.
    pub seat: Seat<State>,
    pub pointer: PointerHandle<State>,
    // Pinch and swipe as sequences a client can follow, rather than as scroll it has to
    // guess at. Held for the lifetime of the program to keep the global registered.
    #[allow(dead_code)]
    pub pointer_gestures_state: PointerGesturesState,
    pub keyboard: KeyboardHandle<State>,
    pub dmabuf_state: DmabufState,
    #[allow(dead_code)]
    pub dmabuf_global: DmabufGlobal,
    // Clipboard and drag and drop. Real toolkits expect this to exist: GTK builds
    // an incomplete GdkSeat without a data device, which is what Chromium's
    // "gdk_seat_get_keyboard" assertion was, and no client can copy or paste
    // without it.
    //
    // Deliberately no primary selection (zwp_primary_selection): its only purpose
    // is pasting on middle click, which we do not want. Without the global,
    // clients have nothing to paste from and middle click stays free for
    // open-in-tab and the canvas.
    pub data_device_state: DataDeviceState,
    // Popup tracking (menus). Smithay owns the tree, the positioner maths and
    // the parent relationships; we only ask it where they go.
    pub popups: PopupManager,
    // Focus handed between clients, which is the one case our own rule cannot cover: a window
    // opened by a different application than the one you were using. Smithay mints the tokens
    // and matches them back; the policy about which to honour is ours, below.
    pub xdg_activation_state: XdgActivationState,

    // Surfaces that committed since the last drain. The render side reads and
    // clears this each frame. Plain surface handles, no rendering state here.
    pub committed: Vec<WlSurface>,
    // wl_buffers destroyed by clients; the render side evicts their cached
    // EGLImages. Drained each frame.
    pub dead_dmabufs: Vec<ObjectId>,
    // The cursor a client asked for, from wl_pointer.set_cursor. Read each frame by the
    // cursor plane code, which decides whether to honour it.
    pub cursor_image: CursorImageStatus,
    // Clients asking to be moved or resized, from dragging their own titlebar or edge.
    // Drained each frame by the canvas code, which is where a drag actually lives. The
    // resize entries carry which edges are moving, as a direction per axis.
    // Each carries the serial of the press that began it, which is what the compositor is
    // meant to anchor the drag to: the request arrives a client's worth of latency after
    // that press, and by then the pointer has moved on.
    pub move_requests: Vec<(WlSurface, u32)>,
    pub resize_requests: Vec<(WlSurface, i32, i32, u32)>,
    // Clients asking to be maximized or unmaximized: the surface, and which of the two. True
    // is maximize. Queued rather than answered here, because what maximizing means is a
    // question about the canvas and this is the protocol boundary.
    pub maximize_requests: Vec<(WlSurface, bool)>,
    // Surfaces a client has asked us to activate with a token we accepted. Drained each frame
    // by the main loop, which is where focus lives.
    pub activation_requests: Vec<WlSurface>,
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
    let xdg_activation_state = XdgActivationState::new::<State>(&dh);

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
    let pointer_gestures_state = PointerGesturesState::new::<State>(&dh);
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
        display_handle: dh.clone(),
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
        pointer_gestures_state,
        keyboard,
        dmabuf_state,
        dmabuf_global,
        data_device_state,
        popups: PopupManager::default(),
        xdg_activation_state,
        committed: Vec::new(),
        dead_dmabufs: Vec::new(),
        cursor_image: CursorImageStatus::default_named(),
        move_requests: Vec::new(),
        resize_requests: Vec::new(),
        maximize_requests: Vec::new(),
        activation_requests: Vec::new(),
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

// Ask a client to be a different size. This is what resizing a window is on Wayland:
// there is nothing to stretch, only a configure telling the client what we want, which
// it renders at and acks in its own time. The client is allowed to refuse, and a
// well-behaved one will clamp to whatever it declared through set_min_size and
// set_max_size, so those bounds are read here rather than assumed.
//
// Called every frame of a resize drag: Smithay only puts a configure on the wire when
// the pending state actually differs, so a drag that has not moved sends nothing.
// Tell a toplevel whether it has the keyboard.
//
// A client draws its own chrome, and it draws it greyed out with no focus ring until it is
// told otherwise, because Activated is the only way it can know. Nothing set it, so every
// window has been rendering itself as the inactive one since the first client connected.
//
// Smithay only puts a configure on the wire when the pending state actually differs, so
// calling this for a window that is already in the state asked for costs nothing.
pub fn set_activated(state: &State, surface: &WlSurface, active: bool) {
    let Some(toplevel) = state
        .xdg_state
        .toplevel_surfaces()
        .iter()
        .find(|t| t.wl_surface() == surface)
    else {
        return;
    };
    toplevel.with_pending_state(|pending| {
        if active {
            pending.states.set(ToplevelState::Activated);
        } else {
            pending.states.unset(ToplevelState::Activated);
        }
    });
    toplevel.send_pending_configure();
}

// resizing says whether this configure is one of a stream. A client that knows it is being
// dragged takes a cheaper relayout path and skips the animations it would otherwise play on
// a size change, which is the difference between a drag that keeps up and one that does not.
// It has to be cleared on the last configure, so the client lays out properly once it stops.
pub fn resize_toplevel(state: &State, surface: &WlSurface, w: i32, h: i32, resizing: bool) {
    let Some(toplevel) = state
        .xdg_state
        .toplevel_surfaces()
        .iter()
        .find(|t| t.wl_surface() == surface)
    else {
        return;
    };
    let (min, max) = size_limits(surface);
    let w = clamp_dim(w, min.0, max.0);
    let h = clamp_dim(h, min.1, max.1);
    toplevel.with_pending_state(|pending| {
        pending.size = Some((w, h).into());
        if resizing {
            pending.states.set(ToplevelState::Resizing);
        } else {
            pending.states.unset(ToplevelState::Resizing);
        }
    });
    toplevel.send_pending_configure();
}

// Tell a toplevel it is maximized, or that it is not, and what size to be while it is.
//
// The state matters as much as the size: a client draws itself differently when it believes it
// is maximized, squaring its corners and turning its maximize button into a restore one, and
// it has no other way to know. Sized like a resize, through the client's own declared limits,
// because a window that refuses to be as large as the view is still entitled to refuse.
pub fn maximize_toplevel(state: &State, surface: &WlSurface, w: i32, h: i32, maximized: bool) {
    let Some(toplevel) = state
        .xdg_state
        .toplevel_surfaces()
        .iter()
        .find(|t| t.wl_surface() == surface)
    else {
        return;
    };
    let (min, max) = size_limits(surface);
    let w = clamp_dim(w, min.0, max.0);
    let h = clamp_dim(h, min.1, max.1);
    toplevel.with_pending_state(|pending| {
        pending.size = Some((w, h).into());
        if maximized {
            pending.states.set(ToplevelState::Maximized);
        } else {
            pending.states.unset(ToplevelState::Maximized);
        }
    });
    // Unconditional, unlike the pending version: a client that asked is owed an answer even
    // when nothing about its state changed, or it sits waiting on a configure that never comes.
    if toplevel.send_pending_configure().is_none() {
        toplevel.send_configure();
    }
}

// What the client said it can be. Zero in either direction means it did not say, which
// Wayland spells as "no limit".
pub fn size_limits(surface: &WlSurface) -> ((i32, i32), (i32, i32)) {
    with_states(surface, |states| {
        let mut cached = states.cached_state.get::<SurfaceCachedState>();
        let current = cached.current();
        (
            (current.min_size.w, current.min_size.h),
            (current.max_size.w, current.max_size.h),
        )
    })
}

fn clamp_dim(v: i32, min: i32, max: i32) -> i32 {
    let mut v = v;
    if min > 0 {
        v = v.max(min);
    }
    if max > 0 {
        v = v.min(max);
    }
    v
}

// Take the clipboard away from whoever had it.
//
// The other half of focus_changed, which Smithay does not call when focus is unset: it sends
// the leave and stops there. Without this, dropping focus entirely (clicking empty canvas, or
// Super+Escape) left the last window still holding the selection offer and able to go on
// reading it.
pub fn clear_clipboard_focus(state: &State) {
    set_data_device_focus(&state.display_handle, &state.seat, None);
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

// The window geometry a client set with xdg_surface::set_window_geometry: the
// rectangle of its surface that is actually the window, as offset and size. What
// is outside it is decoration a client wants ignored, usually a drop shadow, and
// treating it as part of the window makes shadows clickable and misaligns anything
// anchored to a window's edge. None when the client never set one, in which case
// the whole surface is the window.
pub fn geometry_of(surface: &WlSurface) -> Option<(f32, f32, f32, f32)> {
    with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceCachedState>()
            .current()
            .geometry
            .map(|g| {
                (g.loc.x as f32, g.loc.y as f32, g.size.w as f32, g.size.h as f32)
            })
    })
}

fn geometry_loc(surface: &WlSurface) -> (f32, f32) {
    geometry_of(surface).map(|(x, y, _, _)| (x, y)).unwrap_or((0.0, 0.0))
}

// Every surface in a window's tree, bottom to top, each with its offset from the
// root. The order is the stacking order the client asked for: Wayland lets a
// subsurface sit *below* its parent, and Smithay records that by putting each
// surface inside its own children list, so the slot the root occupies is what
// separates what is behind it from what is in front. The root is included, at its
// own slot, so callers can tell the two apart.
//
// The position has to be read in the traversal's processor rather than its filter:
// the filter runs pre-order, the processor runs at the surface's real slot.
pub fn surface_tree(root: &WlSurface) -> Vec<(WlSurface, f32, f32)> {
    let found = std::cell::RefCell::new(Vec::new());
    with_surface_tree_downward(
        root,
        (0i32, 0i32),
        |_, states, parent| TraversalAction::DoChildren(subsurface_offset(states, *parent)),
        |surface, states, parent| {
            let (x, y) = subsurface_offset(states, *parent);
            found.borrow_mut().push((surface.clone(), x as f32, y as f32));
        },
        |_, _, _| true,
    );
    found.into_inner()
}

// A surface's offset from the root: its parent's, plus its own subsurface
// location. Roots have no subsurface state, so they contribute nothing.
fn subsurface_offset(
    states: &smithay::wayland::compositor::SurfaceData,
    parent: (i32, i32),
) -> (i32, i32) {
    if states.cached_state.has::<SubsurfaceCachedState>() {
        let mut guard = states.cached_state.get::<SubsurfaceCachedState>();
        let loc = guard.current().location;
        (parent.0 + loc.x, parent.1 + loc.y)
    } else {
        parent
    }
}

// Up out of any subsurfaces, to the toplevel or popup the surface hangs off.
fn focus_root(surface: &WlSurface) -> WlSurface {
    let mut current = surface.clone();
    while is_subsurface(&current) {
        match get_parent(&current) {
            Some(parent) => current = parent,
            None => break,
        }
    }
    current
}

// The toplevel a surface ultimately belongs to, for raising the right window when a
// click lands on one of its popups or subsurfaces.
pub fn window_root(state: &State, surface: &WlSurface) -> WlSurface {
    let root = focus_root(surface);
    if let Some(popup) = state.popups.find_popup(&root) {
        if let Ok(toplevel) = find_popup_root_surface(&popup) {
            return toplevel;
        }
    }
    root
}

// True when the surface is a subsurface of something else.
pub fn is_subsurface(surface: &WlSurface) -> bool {
    get_role(surface) == Some("subsurface")
}

// Whether a point inside a surface is one the client wants input for. A client can
// declare an input region smaller than its buffer, and a transparent surface with
// no input region would otherwise swallow clicks meant for what is behind it.
pub fn input_region_contains(surface: &WlSurface, local_x: f32, local_y: f32) -> bool {
    with_states(surface, |states| {
        let mut guard = states.cached_state.get::<SurfaceAttributes>();
        match &guard.current().input_region {
            Some(region) => {
                let point =
                    Point::<i32, Logical>::from((local_x as i32, local_y as i32));
                region.contains(point)
            }
            None => true,
        }
    })
}

// The exact surface under a point inside a window or popup, descending into
// subsurfaces, with that surface's offset from the root. Pointer events have to go
// to the surface actually under the cursor: keyboard focus is per window, which is
// why keyboard menu selection works while a click sent to a parent surface, when
// the content lives in a child, arrives nowhere.
//
// Respects each child's input region, so a client that keeps its content in the
// root surface simply gets the root back.
pub fn surface_under(
    root: &WlSurface,
    local_x: f32,
    local_y: f32,
) -> Option<(WlSurface, f32, f32)> {
    let point = Point::<f64, Logical>::from((local_x as f64, local_y as f64));
    under_from_surface_tree(root, point, (0, 0), WindowSurfaceType::ALL)
        .map(|(surface, offset)| (surface, offset.x as f32, offset.y as f32))
}

// If the surface has a newly committed shm buffer, invoke f with its
// dimensions, stride and a pointer to the pixel data (valid only for the call),
// then release the buffer and clear the pending assignment. Returns true when a
// shm buffer was handled.
// Whether this surface is something we draw as a quad. Anything else that commits a buffer
// is a cursor, a drag icon or similar, and the window path releases those buffers rather
// than keeping them, which is why a cursor's pixels have to be taken before it does.
pub fn is_window_like(state: &State, surface: &WlSurface) -> bool {
    is_toplevel(state, surface) || is_popup(state, surface) || is_subsurface(surface)
}

// Read a cursor surface's pixels without consuming them. A cursor surface is committed
// like any other, but it is not a window: nothing else in om_wm will take its buffer, and
// the buffer has to stay put because a cursor is uploaded again every time it moves shape
// rather than every frame. So this borrows the current buffer and leaves it attached.
// Where a client just moved its cursor buffer, if it moved it at all.
//
// wl_surface.attach carries a position for the new buffer's top-left, relative to the previous
// one, in surface coordinates. A hotspot from set_cursor is in those same surface coordinates,
// so the hotspot inside the buffer is the one we were told minus what the attaches have moved
// it. Smithay hands over the per-commit delta and stores the hotspot verbatim, so the running
// sum is the compositor's to keep.
//
// Almost every toolkit attaches at 0,0 and this is always zero for them. Weston's own clients
// do not, which is the whole reason it exists: their cursors sat a dozen pixels off the pointer.
pub fn cursor_attach_delta(surface: &WlSurface) -> (i32, i32) {
    with_states(surface, |data| {
        let mut guard = data.cached_state.get::<SurfaceAttributes>();
        match guard.current().buffer_delta {
            Some(d) => (d.x, d.y),
            None => (0, 0),
        }
    })
}

pub fn with_cursor_pixels<F>(surface: &WlSurface, mut f: F) -> bool
where
    F: FnMut(i32, i32, i32, *const u8),
{
    with_states(surface, |data| {
        let mut guard = data.cached_state.get::<SurfaceAttributes>();
        let attrs = guard.current();
        let buffer = match &attrs.buffer {
            Some(BufferAssignment::NewBuffer(b)) => b.clone(),
            _ => return false,
        };
        with_buffer_contents(&buffer, |ptr, _len, spec| {
            let base = unsafe { ptr.offset(spec.offset as isize) };
            f(spec.width, spec.height, spec.stride, base);
        })
        .is_ok()
    })
}

// Where in that surface the pointer actually is, as the client declared when it set the
// cursor. Getting this wrong puts the click somewhere other than the tip of the arrow.
pub fn cursor_hotspot(surface: &WlSurface) -> (i32, i32) {
    with_states(surface, |data| {
        data.data_map
            .get::<CursorImageSurfaceData>()
            .map(|attrs| {
                let hotspot = attrs.lock().unwrap().hotspot;
                (hotspot.x, hotspot.y)
            })
            .unwrap_or((0, 0))
    })
}

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
// What happens to the buffer is f's to decide, because only f knows what it managed to do
// with it.
pub enum Keep {
    // Ours until a newer one arrives: we sample this buffer in place every frame we draw it,
    // so handing it back would let the client draw into what we are reading.
    Hold,
    // Done with it, and with whatever was held before it. Either we copied the pixels out or
    // we are deliberately not keeping them, and either way what we draw no longer points at
    // the client's memory.
    Release,
    // Of no use to us, and the previous hold has to stay. An import that failed leaves the
    // window drawing from the buffer before it, which is therefore still being sampled.
    Skip,
}

pub fn take_dmabuf_and_retain<F>(
    state: &mut State,
    surface: &WlSurface,
    mut f: F,
) -> bool
where
    F: FnMut(ObjectId, &DmabufInfo) -> Keep,
{
    let mut new_buffer: Option<WlBuffer> = None;
    // Skip until f says otherwise, which is also the right answer for a commit that carried
    // no dmabuf at all: it leaves any existing hold exactly where it is.
    let mut keep = Keep::Skip;

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

        // Retain instead of release when f asks for it: we go on sampling that buffer every
        // frame it is displayed, so it must not go back to the client until a newer one
        // replaces it (see retain_replace).
        keep = f(key, &info);
        if matches!(keep, Keep::Hold) {
            new_buffer = Some(buffer);
        } else {
            buffer.release();
        }
        attrs.buffer = None;
        true
    });

    match (new_buffer, keep) {
        (Some(buf), _) => retain_replace(&mut state.held_dmabufs, surface, buf),
        // Nothing of ours points at the client's memory any more, so neither should the hold.
        (None, Keep::Release) => release_held_dmabuf(state, surface),
        // Skip, or no dmabuf at all: whatever was held is still what we are drawing.
        (None, _) => {}
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

// Signal every surface in a window's tree, subsurfaces included, that it may render its
// next frame.
//
// A frame callback is requested on the surface that is going to be drawn into, and a client
// that puts its content on a subsurface asks on that subsurface. Answering only the root
// leaves such a client waiting for a callback that never comes, which throttles it down to
// whatever timeout its own toolkit falls back on, or stops it dead. Walked in place rather
// than through surface_tree, which would allocate a Vec per window per frame.
pub fn send_frame_callbacks_tree(root: &WlSurface, time_ms: u32) {
    with_surface_tree_downward(
        root,
        (),
        |_, _, _| TraversalAction::DoChildren(()),
        |_, states, _| {
            let mut guard = states.cached_state.get::<SurfaceAttributes>();
            let attrs = guard.current();
            for cb in attrs.frame_callbacks.drain(..) {
                cb.done(time_ms);
            }
        },
        |_, _, _| true,
    );
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


// Focus travelling between clients, which xdg_shell has no way to express and which we
// deliberately do not let a client take for itself.
//
// The shape of it: an application the user is interacting with asks us for a token, quoting
// the input event that prompted it. We hand back an opaque string only we could have minted.
// It passes that to whatever it launches, and the new application presents it back with the
// surface it wants focused. Because the token can only be produced against a real event on a
// real seat, an application nobody touched cannot produce one, which is what separates
// clicking a link and having the browser come forward from a background process helping
// itself to the keyboard.
//
// Our policy is the two checks the protocol leaves to the compositor: the token has to name
// the event that caused it, and it has to be recent. Anything else is refused and the window
// waits to be clicked, which is where we were before this existed.
impl XdgActivationHandler for State {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        let asked_for = data.serial.is_some();
        let recent = data.timestamp.elapsed() < ACTIVATION_MAX_AGE;
        if asked_for && recent {
            self.activation_requests.push(surface);
        }
        // Spent either way. A token is proof of one user action and stays good for one
        // activation; leaving it in the pool would let a client replay it.
        self.xdg_activation_state.remove_token(&token);
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

    // Point the clipboard at whoever now holds the keyboard.
    //
    // A selection is offered to one client: the focused one. Smithay does not follow the
    // keyboard on its own, it hands you this call and expects it from the focus hook, and we
    // were not making it. So a client could copy, the source was recorded, and then nothing
    // was ever offered the result. Copy in one window and paste in another and nothing
    // arrived; the same inside a single window.
    //
    // Only the regular selection, the one behind ctrl+c and a Copy menu item. Primary
    // selection is a separate protocol and we deliberately do not advertise it, so middle
    // click has nothing to paste from and stays free for the canvas.
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|surface| surface.client());
        set_data_device_focus(&self.display_handle, seat, client);
    }
    // What the client under the pointer wants the cursor to look like: its own surface
    // with a hotspot, a named shape, or nothing at all. Recorded rather than acted on,
    // because drawing it belongs to the cursor plane and this is the protocol boundary.
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_image = image;
    }
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

    // A client asking to be moved or resized, which is what dragging its own titlebar or
    // its own edge means: the chrome belongs to the client, the window does not, so it has
    // to ask. Queued rather than acted on, because the drag lives in the canvas code and
    // this is the protocol boundary; main drains these the way it drains commits.
    fn move_request(&mut self, surface: ToplevelSurface, _seat: WlSeat, serial: Serial) {
        self.move_requests
            .push((surface.wl_surface().clone(), u32::from(serial)));
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        _seat: WlSeat,
        serial: Serial,
        edges: ResizeEdge,
    ) {
        // Which edges the client is dragging, as a direction per axis: 1 for the far edge
        // (right, bottom), -1 for the near one, 0 for an axis it is not resizing. The near
        // edges are the interesting case, since moving one has to move the window's origin
        // as well as change its size.
        let (ex, ey) = match edges {
            ResizeEdge::Right => (1, 0),
            ResizeEdge::Left => (-1, 0),
            ResizeEdge::Bottom => (0, 1),
            ResizeEdge::Top => (0, -1),
            ResizeEdge::BottomRight => (1, 1),
            ResizeEdge::BottomLeft => (-1, 1),
            ResizeEdge::TopRight => (1, -1),
            ResizeEdge::TopLeft => (-1, -1),
            // None, or something added to the protocol later: treat it as the corner a
            // Super+drag would use rather than ignoring the request.
            _ => (1, 1),
        };
        self.resize_requests
            .push((surface.wl_surface().clone(), ex, ey, u32::from(serial)));
    }

    // A client's own maximize button, and the double click on its titlebar that means the
    // same thing: the chrome is the client's, so both arrive here as a request rather than as
    // a click we could see. Queued alongside the move and resize ones and drained in main.
    //
    // Nothing is answered here, not even a refusal. The protocol wants a configure either way,
    // and main sends it: it is the only place that knows what shape the view is.
    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.maximize_requests.push((surface.wl_surface().clone(), true));
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.maximize_requests.push((surface.wl_surface().clone(), false));
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

    // A menu asking to own the input. Honouring this is what makes a popup behave
    // like a menu: Smithay's grab routes pointer and keyboard to the popup chain,
    // keeps other windows out of it, gives the popup keyboard focus so arrows and
    // Escape work, and dismisses the chain in protocol order when a click lands
    // outside. Without it we were guessing all of that from hit tests.
    //
    // The serial ties the grab to the input event that caused the menu. A client
    // whose grab collides with an existing one gets refused, which is the check
    // that stops a window quietly swallowing input.
    fn grab(&mut self, surface: PopupSurface, seat: WlSeat, serial: Serial) {
        let Some(seat) = Seat::<State>::from_resource(&seat) else { return };
        let popup = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&popup) else { return };
        let Ok(mut grab) = self.popups.grab_popup(root, popup, &seat, serial) else {
            return;
        };

        if let Some(keyboard) = seat.get_keyboard() {
            let ours = keyboard.has_grab(serial)
                || keyboard.has_grab(grab.previous_serial().unwrap_or(serial));
            if keyboard.is_grabbed() && !ours {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            let ours = pointer.has_grab(serial)
                || pointer.has_grab(grab.previous_serial().unwrap_or(grab.serial()));
            if pointer.is_grabbed() && !ours {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }

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
delegate_xdg_activation!(State);
delegate_pointer_gestures!(State);
