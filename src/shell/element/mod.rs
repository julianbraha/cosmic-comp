use crate::{
    backend::render::{element::AsGlowRenderer, GlMultiFrame, GlMultiRenderer},
    state::State,
    utils::prelude::SeatExt,
};
use id_tree::NodeId;
use smithay::{
    backend::{
        input::KeyState,
        renderer::{
            element::{AsRenderElements, Element, RenderElement, UnderlyingStorage},
            glow::GlowRenderer,
            ImportAll, Renderer,
        },
    },
    desktop::{space::SpaceElement, PopupManager, Window, WindowSurfaceType},
    input::{
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, MotionEvent, PointerTarget},
        Seat,
    },
    output::Output,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState,
        wayland_server::{backend::ObjectId, protocol::wl_surface::WlSurface},
    },
    space_elements,
    utils::{
        Buffer as BufferCoords, IsAlive, Logical, Physical, Point, Rectangle, Scale, Serial, Size,
    },
    wayland::{
        compositor::{with_states, with_surface_tree_downward, TraversalAction},
        seat::WaylandFocus,
        shell::xdg::XdgToplevelSurfaceRoleAttributes,
    },
};
use std::{
    collections::HashMap,
    fmt,
    hash::Hash,
    sync::{Arc, Mutex},
};

pub mod stack;
pub use self::stack::CosmicStack;
pub mod window;
pub use self::window::CosmicWindow;

#[cfg(feature = "debug")]
use crate::backend::render::element::AsGlowFrame;
#[cfg(feature = "debug")]
use egui::plot::{Corner, Legend, Plot, PlotPoints, Polygon};
#[cfg(feature = "debug")]
use smithay::{
    backend::renderer::{
        element::texture::TextureRenderElement, gles2::Gles2Texture, multigpu::Error as MultiError,
    },
    wayland::shell::xdg::XdgToplevelSurfaceData,
};

use super::{focus::FocusDirection, layout::floating::ResizeState};

space_elements! {
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    CosmicMappedInternal;
    Window=CosmicWindow,
    Stack=CosmicStack,
}

#[derive(Clone)]
pub struct CosmicMapped {
    element: CosmicMappedInternal,

    // associated data
    last_cursor_position: Arc<Mutex<HashMap<usize, Point<f64, Logical>>>>,

    //tiling
    pub(super) tiling_node_id: Arc<Mutex<Option<NodeId>>>,
    //floating
    pub(super) last_geometry: Arc<Mutex<Option<Rectangle<i32, Logical>>>>,
    pub(super) resize_state: Arc<Mutex<Option<ResizeState>>>,

    #[cfg(feature = "debug")]
    debug: Arc<Mutex<Option<smithay_egui::EguiState>>>,
}

impl fmt::Debug for CosmicMapped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CosmicMapped")
            .field("element", &self.element)
            .field("last_cursor_position", &self.last_cursor_position)
            .field("tiling_node_id", &self.tiling_node_id)
            .field("resize_state", &self.resize_state)
            .finish()
    }
}

impl PartialEq for CosmicMapped {
    fn eq(&self, other: &Self) -> bool {
        self.element == other.element
    }
}

impl Eq for CosmicMapped {}

impl Hash for CosmicMapped {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.element.hash(state)
    }
}

impl CosmicMapped {
    pub fn windows(&self) -> impl Iterator<Item = (Window, Point<i32, Logical>)> + '_ {
        match &self.element {
            CosmicMappedInternal::Stack(stack) => Box::new(stack.windows().map(|w| {
                (
                    w,
                    stack
                        .header
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|header| Point::from((0, header.height() as i32)))
                        .unwrap_or(Point::from((0, 0))),
                )
            }))
                as Box<dyn Iterator<Item = (Window, Point<i32, Logical>)>>,
            CosmicMappedInternal::Window(window) => Box::new(std::iter::once((
                window.window.clone(),
                window
                    .header
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|header| Point::from((0, header.height() as i32)))
                    .unwrap_or(Point::from((0, 0))),
            ))),
            _ => Box::new(std::iter::empty()),
        }
    }

    pub fn active_window(&self) -> Window {
        match &self.element {
            CosmicMappedInternal::Stack(stack) => stack.active(),
            CosmicMappedInternal::Window(win) => win.window.clone(),
            _ => unreachable!(),
        }
    }

    pub fn active_window_offset(&self) -> Rectangle<i32, Logical> {
        match &self.element {
            CosmicMappedInternal::Stack(stack) => {
                let location = (
                    0,
                    stack
                        .header
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map_or(0, |header| header.height()),
                );
                let size = stack.active().geometry().size;
                Rectangle::from_loc_and_size(location, size)
            }
            CosmicMappedInternal::Window(win) => {
                let location = (
                    0,
                    win.header
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map_or(0, |header| header.height()),
                );
                let size = win.window.geometry().size;
                Rectangle::from_loc_and_size(location, size)
            }
            _ => unreachable!(),
        }
    }

    pub fn cursor_position(&self, seat: &Seat<State>) -> Option<Point<f64, Logical>> {
        self.last_cursor_position
            .lock()
            .unwrap()
            .get(&seat.id())
            .cloned()
    }

    pub fn set_active(&self, window: &Window) {
        if let CosmicMappedInternal::Stack(stack) = &self.element {
            stack.set_active(window);
        }
    }

    pub fn focus_window(&self, window: &Window) {
        match &self.element {
            CosmicMappedInternal::Stack(stack) => stack.set_active(window),
            _ => {}
        }
    }

    pub fn has_surface(&self, surface: &WlSurface, surface_type: WindowSurfaceType) -> bool {
        self.windows().any(|(w, _)| {
            let toplevel = w.toplevel().wl_surface();

            if surface_type.contains(WindowSurfaceType::TOPLEVEL) {
                if toplevel == surface {
                    return true;
                }
            }

            if surface_type.contains(WindowSurfaceType::SUBSURFACE) {
                use std::sync::atomic::{AtomicBool, Ordering};

                let found = AtomicBool::new(false);
                with_surface_tree_downward(
                    toplevel,
                    surface,
                    |_, _, search| TraversalAction::DoChildren(search),
                    |s, _, search| {
                        found.fetch_or(s == *search, Ordering::SeqCst);
                    },
                    |_, _, _| !found.load(Ordering::SeqCst),
                );
                if found.load(Ordering::SeqCst) {
                    return true;
                }
            }

            if surface_type.contains(WindowSurfaceType::POPUP) {
                PopupManager::popups_for_surface(toplevel).any(|(p, _)| p.wl_surface() == surface)
            } else {
                false
            }
        })
    }

    pub fn handle_focus(&self, direction: FocusDirection) -> bool {
        if let CosmicMappedInternal::Stack(stack) = &self.element {
            //TODO: stack.handle_focus(direction)
            false
        } else {
            false
        }
    }

    pub fn set_resizing(&self, resizing: bool) {
        for window in match &self.element {
            CosmicMappedInternal::Stack(s) => {
                Box::new(s.windows()) as Box<dyn Iterator<Item = Window>>
            }
            CosmicMappedInternal::Window(w) => Box::new(std::iter::once(w.window.clone())),
            _ => unreachable!(),
        } {
            window.toplevel().with_pending_state(|state| {
                if resizing {
                    state.states.set(XdgState::Resizing);
                } else {
                    state.states.unset(XdgState::Resizing);
                }
            });
        }
    }

    pub fn is_resizing(&self) -> bool {
        let window = match &self.element {
            CosmicMappedInternal::Stack(s) => s.active(),
            CosmicMappedInternal::Window(w) => w.window.clone(),
            _ => unreachable!(),
        };

        let xdg = window.toplevel();
        xdg.current_state().states.contains(XdgState::Resizing)
            || xdg.with_pending_state(|states| states.states.contains(XdgState::Resizing))
    }

    pub fn set_tiled(&self, tiled: bool) {
        for xdg in match &self.element {
            // we use the tiled state of stack windows anyway to get rid of decorations
            CosmicMappedInternal::Stack(_) => None,
            CosmicMappedInternal::Window(w) => Some(w.window.toplevel()),
            _ => unreachable!(),
        } {
            xdg.with_pending_state(|state| {
                if tiled {
                    state.states.set(XdgState::TiledLeft);
                    state.states.set(XdgState::TiledRight);
                    state.states.set(XdgState::TiledTop);
                    state.states.set(XdgState::TiledBottom);
                } else {
                    state.states.unset(XdgState::TiledLeft);
                    state.states.unset(XdgState::TiledRight);
                    state.states.unset(XdgState::TiledTop);
                    state.states.unset(XdgState::TiledBottom);
                }
            });
        }
    }

    pub fn is_tiled(&self) -> bool {
        let window = match &self.element {
            CosmicMappedInternal::Stack(s) => s.active(),
            CosmicMappedInternal::Window(w) => w.window.clone(),
            _ => unreachable!(),
        };

        window
            .toplevel()
            .current_state()
            .states
            .contains(XdgState::TiledLeft)
    }

    pub fn set_fullscreen(&self, fullscreen: bool) {
        for window in match &self.element {
            CosmicMappedInternal::Stack(s) => {
                Box::new(s.windows()) as Box<dyn Iterator<Item = Window>>
            }
            CosmicMappedInternal::Window(w) => Box::new(std::iter::once(w.window.clone())),
            _ => unreachable!(),
        } {
            window.toplevel().with_pending_state(|state| {
                if fullscreen {
                    state.states.set(XdgState::Fullscreen);
                } else {
                    state.states.unset(XdgState::Fullscreen);
                }
            });
        }
    }

    pub fn is_fullscreen(&self) -> bool {
        let window = match &self.element {
            CosmicMappedInternal::Stack(s) => s.active(),
            CosmicMappedInternal::Window(w) => w.window.clone(),
            _ => unreachable!(),
        };

        let xdg = window.toplevel();
        xdg.current_state().states.contains(XdgState::Fullscreen)
            || xdg.with_pending_state(|states| states.states.contains(XdgState::Fullscreen))
    }

    pub fn set_maximized(&self, maximized: bool) {
        for window in match &self.element {
            CosmicMappedInternal::Stack(s) => {
                Box::new(s.windows()) as Box<dyn Iterator<Item = Window>>
            }
            CosmicMappedInternal::Window(w) => Box::new(std::iter::once(w.window.clone())),
            _ => unreachable!(),
        } {
            window.toplevel().with_pending_state(|state| {
                if maximized {
                    state.states.set(XdgState::Maximized);
                } else {
                    state.states.unset(XdgState::Maximized);
                }
            });
        }
    }

    pub fn is_maximized(&self) -> bool {
        let window = match &self.element {
            CosmicMappedInternal::Stack(s) => s.active(),
            CosmicMappedInternal::Window(w) => w.window.clone(),
            _ => unreachable!(),
        };

        let xdg = window.toplevel();
        xdg.current_state().states.contains(XdgState::Maximized)
            || xdg.with_pending_state(|states| states.states.contains(XdgState::Maximized))
    }

    pub fn set_activated(&self, activated: bool) {
        for window in match &self.element {
            CosmicMappedInternal::Stack(s) => {
                Box::new(s.windows()) as Box<dyn Iterator<Item = Window>>
            }
            CosmicMappedInternal::Window(w) => Box::new(std::iter::once(w.window.clone())),
            _ => unreachable!(),
        } {
            window.toplevel().with_pending_state(|state| {
                if activated {
                    state.states.set(XdgState::Activated);
                } else {
                    state.states.unset(XdgState::Activated);
                }
            });
        }
    }

    pub fn is_activated(&self) -> bool {
        let window = match &self.element {
            CosmicMappedInternal::Stack(s) => s.active(),
            CosmicMappedInternal::Window(w) => w.window.clone(),
            _ => unreachable!(),
        };

        let xdg = window.toplevel();
        xdg.current_state().states.contains(XdgState::Activated)
            || xdg.with_pending_state(|states| states.states.contains(XdgState::Activated))
    }

    pub fn set_size(&self, size: Size<i32, Logical>) {
        match &self.element {
            CosmicMappedInternal::Stack(s) => s.set_size(size),
            CosmicMappedInternal::Window(w) => w.set_size(size),
            _ => {}
        }
    }

    pub fn min_size(&self) -> Size<i32, Logical> {
        match &self.element {
            CosmicMappedInternal::Stack(stack) => stack
                .windows()
                .fold(None, |min_size, window| {
                    let win_min_size = with_states(window.toplevel().wl_surface(), |states| {
                        let attrs = states
                            .data_map
                            .get::<Mutex<XdgToplevelSurfaceRoleAttributes>>()
                            .unwrap()
                            .lock()
                            .unwrap();
                        attrs.min_size
                    });
                    match (min_size, win_min_size) {
                        (None, x) => Some(x),
                        (Some(min1), min2) => Some((min1.w.max(min2.w), min1.h.max(min2.h)).into()),
                    }
                })
                .expect("Empty stack?"),
            CosmicMappedInternal::Window(window) => {
                with_states(window.window.toplevel().wl_surface(), |states| {
                    let attrs = states
                        .data_map
                        .get::<Mutex<XdgToplevelSurfaceRoleAttributes>>()
                        .unwrap()
                        .lock()
                        .unwrap();
                    attrs.min_size
                })
            }
            _ => unreachable!(),
        }
    }

    pub fn max_size(&self) -> Size<i32, Logical> {
        match &self.element {
            CosmicMappedInternal::Stack(stack) => {
                let theoretical_max = stack.windows().fold(None, |max_size, window| {
                    let win_max_size = with_states(window.toplevel().wl_surface(), |states| {
                        let attrs = states
                            .data_map
                            .get::<Mutex<XdgToplevelSurfaceRoleAttributes>>()
                            .unwrap()
                            .lock()
                            .unwrap();
                        attrs.max_size
                    });
                    match (max_size, win_max_size) {
                        (None, x) => Some(x),
                        (Some(max1), max2) => Some(
                            (
                                if max1.w == 0 {
                                    max2.w
                                } else if max2.w == 0 {
                                    max1.w
                                } else {
                                    max1.w.min(max2.w)
                                },
                                if max1.h == 0 {
                                    max2.h
                                } else if max2.h == 0 {
                                    max1.h
                                } else {
                                    max1.h.min(max2.h)
                                },
                            )
                                .into(),
                        ),
                    }
                });
                // The problem is, with accumulated sizes, the minimum size could be larger than our maximum...
                let min_size = self.min_size();
                match (theoretical_max, min_size) {
                    (None, _) => (0, 0).into(),
                    (Some(max), min) => (max.w.max(min.w), max.h.max(min.h)).into(),
                }
            }
            CosmicMappedInternal::Window(window) => {
                with_states(window.window.toplevel().wl_surface(), |states| {
                    let attrs = states
                        .data_map
                        .get::<Mutex<XdgToplevelSurfaceRoleAttributes>>()
                        .unwrap()
                        .lock()
                        .unwrap();
                    attrs.max_size
                })
            }
            _ => unreachable!(),
        }
    }

    pub fn configure(&self) {
        for window in match &self.element {
            CosmicMappedInternal::Stack(s) => {
                Box::new(s.windows()) as Box<dyn Iterator<Item = Window>>
            }
            CosmicMappedInternal::Window(w) => Box::new(std::iter::once(w.window.clone())),
            _ => unreachable!(),
        } {
            window.toplevel().send_configure();
        }
    }

    pub fn send_close(&self) {
        let window = match &self.element {
            CosmicMappedInternal::Stack(s) => s.active(),
            CosmicMappedInternal::Window(w) => w.window.clone(),
            _ => unreachable!(),
        };

        window.toplevel().send_close();
    }

    #[cfg(feature = "debug")]
    pub fn set_debug(&self, flag: bool) {
        let mut debug = self.debug.lock().unwrap();
        if flag {
            *debug = Some(smithay_egui::EguiState::new(Rectangle::from_loc_and_size(
                (10, 10),
                (100, 100),
            )));
        } else {
            debug.take();
        }
    }
}

impl IsAlive for CosmicMapped {
    fn alive(&self) -> bool {
        self.element.alive()
    }
}

impl SpaceElement for CosmicMapped {
    fn bbox(&self) -> Rectangle<i32, Logical> {
        SpaceElement::bbox(&self.element)
    }
    fn is_in_input_region(&self, point: &Point<f64, Logical>) -> bool {
        SpaceElement::is_in_input_region(&self.element, point)
    }
    fn set_activate(&self, activated: bool) {
        SpaceElement::set_activate(&self.element, activated)
    }
    fn output_enter(&self, output: &Output, overlap: Rectangle<i32, Logical>) {
        SpaceElement::output_enter(&self.element, output, overlap)
    }
    fn output_leave(&self, output: &Output) {
        SpaceElement::output_leave(&self.element, output)
    }
    fn geometry(&self) -> Rectangle<i32, Logical> {
        SpaceElement::geometry(&self.element)
    }
    fn z_index(&self) -> u8 {
        SpaceElement::z_index(&self.element)
    }
    fn refresh(&self) {
        SpaceElement::refresh(&self.element)
    }
}

impl KeyboardTarget<State> for CosmicMapped {
    fn enter(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        match &self.element {
            CosmicMappedInternal::Stack(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
            CosmicMappedInternal::Window(w) => KeyboardTarget::enter(w, seat, data, keys, serial),
            _ => {}
        }
    }
    fn leave(&self, seat: &Seat<State>, data: &mut State, serial: Serial) {
        match &self.element {
            CosmicMappedInternal::Stack(s) => KeyboardTarget::leave(s, seat, data, serial),
            CosmicMappedInternal::Window(w) => KeyboardTarget::leave(w, seat, data, serial),
            _ => {}
        }
    }
    fn key(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        match &self.element {
            CosmicMappedInternal::Stack(s) => {
                KeyboardTarget::key(s, seat, data, key, state, serial, time)
            }
            CosmicMappedInternal::Window(w) => {
                KeyboardTarget::key(w, seat, data, key, state, serial, time)
            }
            _ => {}
        }
    }
    fn modifiers(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        match &self.element {
            CosmicMappedInternal::Stack(s) => {
                KeyboardTarget::modifiers(s, seat, data, modifiers, serial)
            }
            CosmicMappedInternal::Window(w) => {
                KeyboardTarget::modifiers(w, seat, data, modifiers, serial)
            }
            _ => {}
        }
    }
}

impl PointerTarget<State> for CosmicMapped {
    fn enter(&self, seat: &Seat<State>, data: &mut State, event: &MotionEvent) {
        self.last_cursor_position
            .lock()
            .unwrap()
            .insert(seat.id(), event.location);
        match &self.element {
            CosmicMappedInternal::Stack(s) => PointerTarget::enter(s, seat, data, event),
            CosmicMappedInternal::Window(w) => PointerTarget::enter(w, seat, data, event),
            _ => {}
        }
    }
    fn motion(&self, seat: &Seat<State>, data: &mut State, event: &MotionEvent) {
        self.last_cursor_position
            .lock()
            .unwrap()
            .insert(seat.id(), event.location);
        match &self.element {
            CosmicMappedInternal::Stack(s) => PointerTarget::motion(s, seat, data, event),
            CosmicMappedInternal::Window(w) => PointerTarget::motion(w, seat, data, event),
            _ => {}
        }
    }
    fn button(&self, seat: &Seat<State>, data: &mut State, event: &ButtonEvent) {
        match &self.element {
            CosmicMappedInternal::Stack(s) => PointerTarget::button(s, seat, data, event),
            CosmicMappedInternal::Window(w) => PointerTarget::button(w, seat, data, event),
            _ => {}
        }
    }
    fn axis(&self, seat: &Seat<State>, data: &mut State, frame: AxisFrame) {
        match &self.element {
            CosmicMappedInternal::Stack(s) => PointerTarget::axis(s, seat, data, frame),
            CosmicMappedInternal::Window(w) => PointerTarget::axis(w, seat, data, frame),
            _ => {}
        }
    }
    fn leave(&self, seat: &Seat<State>, data: &mut State, serial: Serial, time: u32) {
        self.last_cursor_position.lock().unwrap().remove(&seat.id());
        match &self.element {
            CosmicMappedInternal::Stack(s) => PointerTarget::leave(s, seat, data, serial, time),
            CosmicMappedInternal::Window(w) => PointerTarget::leave(w, seat, data, serial, time),
            _ => {}
        }
    }
}

impl WaylandFocus for CosmicMapped {
    fn wl_surface(&self) -> Option<WlSurface> {
        match &self.element {
            CosmicMappedInternal::Window(w) => w.window.wl_surface().clone(),
            CosmicMappedInternal::Stack(s) => s.active().wl_surface().clone(),
            _ => None,
        }
    }

    fn same_client_as(&self, object_id: &ObjectId) -> bool {
        match &self.element {
            CosmicMappedInternal::Window(w) => w.window.same_client_as(object_id),
            CosmicMappedInternal::Stack(s) => s.windows().any(|w| w.same_client_as(object_id)),
            _ => false,
        }
    }
}

impl From<CosmicWindow> for CosmicMapped {
    fn from(w: CosmicWindow) -> Self {
        CosmicMapped {
            element: CosmicMappedInternal::Window(w),
            last_cursor_position: Arc::new(Mutex::new(HashMap::new())),
            tiling_node_id: Arc::new(Mutex::new(None)),
            last_geometry: Arc::new(Mutex::new(None)),
            resize_state: Arc::new(Mutex::new(None)),
            #[cfg(feature = "debug")]
            debug: Arc::new(Mutex::new(None)),
        }
    }
}

impl From<CosmicStack> for CosmicMapped {
    fn from(s: CosmicStack) -> Self {
        CosmicMapped {
            element: CosmicMappedInternal::Stack(s),
            last_cursor_position: Arc::new(Mutex::new(HashMap::new())),
            tiling_node_id: Arc::new(Mutex::new(None)),
            last_geometry: Arc::new(Mutex::new(None)),
            resize_state: Arc::new(Mutex::new(None)),
            #[cfg(feature = "debug")]
            debug: Arc::new(Mutex::new(None)),
        }
    }
}

pub enum CosmicMappedRenderElement<R>
where
    R: AsGlowRenderer + Renderer + ImportAll,
    <R as Renderer>::TextureId: 'static,
{
    Stack(self::stack::CosmicStackRenderElement<R>),
    Window(self::window::CosmicWindowRenderElement<R>),
    #[cfg(feature = "debug")]
    Egui(TextureRenderElement<Gles2Texture>),
}

impl<R> Element for CosmicMappedRenderElement<R>
where
    R: AsGlowRenderer + Renderer + ImportAll,
    <R as Renderer>::TextureId: 'static,
{
    fn id(&self) -> &smithay::backend::renderer::element::Id {
        match self {
            CosmicMappedRenderElement::Stack(elem) => elem.id(),
            CosmicMappedRenderElement::Window(elem) => elem.id(),
            #[cfg(feature = "debug")]
            CosmicMappedRenderElement::Egui(elem) => elem.id(),
        }
    }

    fn current_commit(&self) -> smithay::backend::renderer::utils::CommitCounter {
        match self {
            CosmicMappedRenderElement::Stack(elem) => elem.current_commit(),
            CosmicMappedRenderElement::Window(elem) => elem.current_commit(),
            #[cfg(feature = "debug")]
            CosmicMappedRenderElement::Egui(elem) => elem.current_commit(),
        }
    }

    fn src(&self) -> Rectangle<f64, smithay::utils::Buffer> {
        match self {
            CosmicMappedRenderElement::Stack(elem) => elem.src(),
            CosmicMappedRenderElement::Window(elem) => elem.src(),
            #[cfg(feature = "debug")]
            CosmicMappedRenderElement::Egui(elem) => elem.src(),
        }
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        match self {
            CosmicMappedRenderElement::Stack(elem) => elem.geometry(scale),
            CosmicMappedRenderElement::Window(elem) => elem.geometry(scale),
            #[cfg(feature = "debug")]
            CosmicMappedRenderElement::Egui(elem) => elem.geometry(scale),
        }
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        match self {
            CosmicMappedRenderElement::Stack(elem) => elem.location(scale),
            CosmicMappedRenderElement::Window(elem) => elem.location(scale),
            #[cfg(feature = "debug")]
            CosmicMappedRenderElement::Egui(elem) => elem.location(scale),
        }
    }

    fn transform(&self) -> smithay::utils::Transform {
        match self {
            CosmicMappedRenderElement::Stack(elem) => elem.transform(),
            CosmicMappedRenderElement::Window(elem) => elem.transform(),
            #[cfg(feature = "debug")]
            CosmicMappedRenderElement::Egui(elem) => elem.transform(),
        }
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<smithay::backend::renderer::utils::CommitCounter>,
    ) -> Vec<Rectangle<i32, Physical>> {
        match self {
            CosmicMappedRenderElement::Stack(elem) => elem.damage_since(scale, commit),
            CosmicMappedRenderElement::Window(elem) => elem.damage_since(scale, commit),
            #[cfg(feature = "debug")]
            CosmicMappedRenderElement::Egui(elem) => elem.damage_since(scale, commit),
        }
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> Vec<Rectangle<i32, Physical>> {
        match self {
            CosmicMappedRenderElement::Stack(elem) => elem.opaque_regions(scale),
            CosmicMappedRenderElement::Window(elem) => elem.opaque_regions(scale),
            #[cfg(feature = "debug")]
            CosmicMappedRenderElement::Egui(elem) => elem.opaque_regions(scale),
        }
    }
}

impl RenderElement<GlowRenderer> for CosmicMappedRenderElement<GlowRenderer> {
    fn draw<'frame>(
        &self,
        frame: &mut <GlowRenderer as Renderer>::Frame<'frame>,
        src: Rectangle<f64, BufferCoords>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        log: &slog::Logger,
    ) -> Result<(), <GlowRenderer as Renderer>::Error> {
        match self {
            CosmicMappedRenderElement::Stack(elem) => elem.draw(frame, src, dst, damage, log),
            CosmicMappedRenderElement::Window(elem) => elem.draw(frame, src, dst, damage, log),
            #[cfg(feature = "debug")]
            CosmicMappedRenderElement::Egui(elem) => {
                RenderElement::<GlowRenderer>::draw(elem, frame, location, scale, damage, log)
            }
        }
    }

    fn underlying_storage(
        &self,
        renderer: &GlowRenderer,
    ) -> Option<UnderlyingStorage<'_, GlowRenderer>> {
        match self {
            CosmicMappedRenderElement::Stack(elem) => elem.underlying_storage(renderer),
            CosmicMappedRenderElement::Window(elem) => elem.underlying_storage(renderer),
            #[cfg(feature = "debug")]
            CosmicMappedRenderElement::Egui(elem) => elem.underlying_storage(renderer),
        }
    }
}

impl<'a> RenderElement<GlMultiRenderer<'a>> for CosmicMappedRenderElement<GlMultiRenderer<'a>> {
    fn draw<'frame>(
        &self,
        frame: &mut GlMultiFrame<'a, 'frame>,
        src: Rectangle<f64, BufferCoords>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        log: &slog::Logger,
    ) -> Result<(), <GlMultiRenderer<'_> as Renderer>::Error> {
        match self {
            CosmicMappedRenderElement::Stack(elem) => elem.draw(frame, src, dst, damage, log),
            CosmicMappedRenderElement::Window(elem) => elem.draw(frame, src, dst, damage, log),
            #[cfg(feature = "debug")]
            CosmicMappedRenderElement::Egui(elem) => {
                let glow_frame = frame.glow_frame_mut();
                RenderElement::<GlowRenderer>::draw(elem, glow_frame, location, scale, damage, log)
                    .map_err(|err| MultiError::Render(err))
            }
        }
    }

    fn underlying_storage(
        &self,
        renderer: &GlMultiRenderer<'a>,
    ) -> Option<UnderlyingStorage<'_, GlMultiRenderer<'a>>> {
        match self {
            CosmicMappedRenderElement::Stack(elem) => elem.underlying_storage(renderer),
            CosmicMappedRenderElement::Window(elem) => elem.underlying_storage(renderer),
            #[cfg(feature = "debug")]
            CosmicMappedRenderElement::Egui(elem) => {
                let glow_renderer = renderer.glow_renderer();
                match elem.underlying_storage(glow_renderer) {
                    Some(UnderlyingStorage::Wayland(buffer)) => {
                        Some(UnderlyingStorage::Wayland(buffer))
                    }
                    _ => None,
                }
            }
        }
    }
}

impl<R> From<stack::CosmicStackRenderElement<R>> for CosmicMappedRenderElement<R>
where
    R: Renderer + ImportAll + AsGlowRenderer,
    <R as Renderer>::TextureId: 'static,
    CosmicMappedRenderElement<R>: RenderElement<R>,
{
    fn from(elem: stack::CosmicStackRenderElement<R>) -> Self {
        CosmicMappedRenderElement::Stack(elem)
    }
}
impl<R> From<window::CosmicWindowRenderElement<R>> for CosmicMappedRenderElement<R>
where
    R: Renderer + ImportAll + AsGlowRenderer,
    <R as Renderer>::TextureId: 'static,
    CosmicMappedRenderElement<R>: RenderElement<R>,
{
    fn from(elem: window::CosmicWindowRenderElement<R>) -> Self {
        CosmicMappedRenderElement::Window(elem)
    }
}
#[cfg(feature = "debug")]
impl<R> From<TextureRenderElement<Gles2Texture>> for CosmicMappedRenderElement<R>
where
    R: Renderer + ImportAll + AsGlowRenderer,
    <R as Renderer>::TextureId: 'static,
    CosmicMappedRenderElement<R>: RenderElement<R>,
{
    fn from(elem: TextureRenderElement<Gles2Texture>) -> Self {
        CosmicMappedRenderElement::Egui(elem)
    }
}

impl<R> AsRenderElements<R> for CosmicMapped
where
    R: Renderer + ImportAll + AsGlowRenderer,
    <R as Renderer>::TextureId: 'static,
    CosmicMappedRenderElement<R>: RenderElement<R>,
{
    type RenderElement = CosmicMappedRenderElement<R>;
    fn render_elements<C: From<Self::RenderElement>>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
    ) -> Vec<C> {
        #[cfg(feature = "debug")]
        let mut elements = if let Some(debug) = self.debug.lock().unwrap().as_mut() {
            let window = self.active_window();
            let window_geo = window.geometry();
            let (app_id, title, min_size, max_size, size, states) =
                with_states(&window.toplevel().wl_surface(), |states| {
                    let attributes = states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .unwrap()
                        .lock()
                        .unwrap();
                    (
                        attributes.app_id.clone(),
                        attributes.title.clone(),
                        attributes.min_size.clone(),
                        attributes.max_size.clone(),
                        attributes.current.size.clone(),
                        attributes.current.states.clone(),
                    )
                });

            let area = Rectangle::<i32, Logical>::from_loc_and_size(
                location.to_f64().to_logical(scale).to_i32_round(),
                self.bbox().size,
            );

            let glow_renderer = renderer.glow_renderer_mut();
            match debug.render(
                |ctx| {
                    egui::Area::new("window")
                        .anchor(
                            egui::Align2::RIGHT_TOP,
                            [
                                -window_geo.loc.x as f32 - 10.0,
                                window_geo.loc.y as f32 - 10.0,
                            ],
                        )
                        .show(ctx, |ui| {
                            egui::Frame::none()
                                .fill(egui::Color32::BLACK)
                                .rounding(5.0)
                                .inner_margin(10.0)
                                .show(ui, |ui| {
                                    ui.heading(title.as_deref().unwrap_or("<None>"));
                                    ui.horizontal(|ui| {
                                        ui.label("App ID: ");
                                        ui.label(app_id.as_deref().unwrap_or("<None>"));
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("States: ");
                                        if states.contains(XdgState::Maximized) {
                                            ui.label("🗖");
                                        }
                                        if states.contains(XdgState::Fullscreen) {
                                            ui.label("⬜");
                                        }
                                        if states.contains(XdgState::Activated) {
                                            ui.label("🖱");
                                        }
                                        if states.contains(XdgState::Resizing) {
                                            ui.label("↔");
                                        }
                                        if states.contains(XdgState::TiledLeft) {
                                            ui.label("⏴");
                                        }
                                        if states.contains(XdgState::TiledRight) {
                                            ui.label("⏵");
                                        }
                                        if states.contains(XdgState::TiledTop) {
                                            ui.label("⏶");
                                        }
                                        if states.contains(XdgState::TiledBottom) {
                                            ui.label("⏷");
                                        }
                                    });

                                    let plot = Plot::new("Sizes")
                                        .legend(Legend::default().position(Corner::RightBottom))
                                        .data_aspect(1.0)
                                        .view_aspect(1.0)
                                        .show_x(false)
                                        .show_y(false)
                                        .width(200.0)
                                        .height(200.0);
                                    plot.show(ui, |plot_ui| {
                                        let center = ((max_size.w + 20) / 2, (max_size.h + 20) / 2);
                                        let max_size_rect = Polygon::new(PlotPoints::new(vec![
                                            [10.0, 10.0],
                                            [max_size.w as f64 + 10.0, 10.0],
                                            [max_size.w as f64 + 10.0, max_size.h as f64 + 10.0],
                                            [10.0, max_size.h as f64 + 10.0],
                                            [10.0, 10.0],
                                        ]));
                                        plot_ui.polygon(
                                            max_size_rect
                                                .name(format!("{}x{}", max_size.w, max_size.h)),
                                        );

                                        if let Some(size) = size {
                                            let size_rect = Polygon::new(PlotPoints::new(vec![
                                                [
                                                    (center.0 - size.w / 2) as f64,
                                                    (center.1 - size.h / 2) as f64,
                                                ],
                                                [
                                                    (center.0 + size.w / 2) as f64,
                                                    (center.1 - size.h / 2) as f64,
                                                ],
                                                [
                                                    (center.0 + size.w / 2) as f64,
                                                    (center.1 + size.h / 2) as f64,
                                                ],
                                                [
                                                    (center.0 - size.w / 2) as f64,
                                                    (center.1 + size.h / 2) as f64,
                                                ],
                                                [
                                                    (center.0 - size.w / 2) as f64,
                                                    (center.1 - size.h / 2) as f64,
                                                ],
                                            ]));
                                            plot_ui.polygon(
                                                size_rect.name(format!("{}x{}", size.w, size.h)),
                                            );
                                        }

                                        let min_size_rect = Polygon::new(PlotPoints::new(vec![
                                            [
                                                (center.0 - min_size.w / 2) as f64,
                                                (center.1 - min_size.h / 2) as f64,
                                            ],
                                            [
                                                (center.0 + min_size.w / 2) as f64,
                                                (center.1 - min_size.h / 2) as f64,
                                            ],
                                            [
                                                (center.0 + min_size.w / 2) as f64,
                                                (center.1 + min_size.h / 2) as f64,
                                            ],
                                            [
                                                (center.0 - min_size.w / 2) as f64,
                                                (center.1 + min_size.h / 2) as f64,
                                            ],
                                            [
                                                (center.0 - min_size.w / 2) as f64,
                                                (center.1 - min_size.h / 2) as f64,
                                            ],
                                        ]));
                                        plot_ui.polygon(
                                            min_size_rect
                                                .name(format!("{}x{}", min_size.w, min_size.h)),
                                        );
                                    })
                                })
                        });
                },
                glow_renderer,
                area,
                scale.x,
                0.8,
            ) {
                Ok(element) => vec![element.into()],
                Err(err) => {
                    slog_scope::debug!("Error rendering debug overlay: {}", err);
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        #[cfg(not(feature = "debug"))]
        let mut elements = Vec::new();

        #[cfg_attr(not(feature = "debug"), allow(unused_mut))]
        match &self.element {
            CosmicMappedInternal::Stack(s) => {
                elements.extend(AsRenderElements::<R>::render_elements::<
                    CosmicMappedRenderElement<R>,
                >(s, renderer, location, scale))
            }
            CosmicMappedInternal::Window(w) => {
                elements.extend(AsRenderElements::<R>::render_elements::<
                    CosmicMappedRenderElement<R>,
                >(w, renderer, location, scale))
            }
            _ => {}
        };

        elements.into_iter().map(C::from).collect()
    }
}
