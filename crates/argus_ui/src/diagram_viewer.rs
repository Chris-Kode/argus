//! A Mermaid diagram on the whole window, to zoom and pan.
//!
//! Clicking a drawn diagram ([`crate::mermaid::OpenDiagram`]) opens it here,
//! fitted to the window. Dragging or scrolling pans; pinching, or
//! scrolling with ⌘ (ctrl elsewhere), zooms around the pointer; the toolbar
//! and `=` / `-` / `0` / `1` zoom in, out, to fit and to the natural size.
//! `esc` closes it.
//!
//! GPUI rasterizes an SVG image at twice its size, so once the zoom needs
//! more pixels than that on this display, the viewer lays a sharper copy ([`Diagram::scaled`]) over the diagram once it is
//! rasterized; the copies are dropped when the viewer closes.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    AnyElement, Context, CursorStyle, EventEmitter, FocusHandle, Focusable, Image, MouseButton,
    ObjectFit, Pixels, Point, ScrollWheelEvent, Size, Window, div, img, point, prelude::*, px,
    size,
};

use crate::components::{button, icon_button, kbd};
use crate::icons::IconName;
use crate::keymap::{DIAGRAM_VIEWER_CONTEXT, ZoomIn, ZoomOut, ZoomToActualSize, ZoomToFit};
use crate::mermaid::Diagram;
use crate::theme::ActiveTheme;

/// Height of the toolbar above the diagram.
const TOOLBAR_HEIGHT: Pixels = px(40.0);
/// Smallest zoom.
const MIN_ZOOM: f32 = 0.1;
/// Largest zoom.
const MAX_ZOOM: f32 = 8.0;
/// Largest zoom when fitting a small diagram to the window.
const MAX_FIT_ZOOM: f32 = 2.0;
/// Zoom factor of one step (button or key).
const ZOOM_STEP: f32 = 1.25;
/// Scrolled pixels that double (or halve) the zoom with ⌘ held.
const SCROLL_PIXELS_PER_DOUBLING: f32 = 300.0;
/// Room kept around a fitted diagram.
const FIT_MARGIN: f32 = 24.0;

/// Events of the viewer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiagramViewerEvent {
    /// The user closed the viewer.
    Closed,
}

/// A drag in progress: where the pointer and the diagram started.
#[derive(Clone, Copy)]
struct Drag {
    pointer: Point<Pixels>,
    offset: Point<Pixels>,
}

/// The diagram viewer.
pub struct DiagramViewer {
    diagram: Arc<Diagram>,
    focus_handle: FocusHandle,
    zoom: f32,
    /// Top-left corner of the diagram inside the canvas.
    offset: Point<Pixels>,
    /// The diagram follows the window size until the user zooms or pans.
    fitted: bool,
    drag: Option<Drag>,
    /// The canvas (the window below the toolbar) at the last render.
    canvas: Size<Pixels>,
    /// Sharper copies of the diagram by scale.
    scaled: HashMap<u32, Arc<Image>>,
}

impl EventEmitter<DiagramViewerEvent> for DiagramViewer {}

impl Focusable for DiagramViewer {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl DiagramViewer {
    /// Opens `diagram`, fitted to the window.
    pub fn new(diagram: Arc<Diagram>, cx: &mut Context<Self>) -> Self {
        Self {
            diagram,
            focus_handle: cx.focus_handle(),
            zoom: 1.0,
            offset: Point::default(),
            fitted: true,
            drag: None,
            canvas: Size::default(),
            scaled: HashMap::new(),
        }
    }

    /// Drops the sharper copies from GPUI's image caches.
    pub fn release(&mut self, window: &mut Window, cx: &mut gpui::App) {
        for (_, image) in self.scaled.drain() {
            if let Some(render) = image.clone().get_render_image(window, cx) {
                window.drop_image(render).ok();
            }
            image.remove_asset(cx);
        }
    }

    fn fit_zoom(&self) -> f32 {
        let width = (self.canvas.width.as_f32() - 2.0 * FIT_MARGIN).max(1.0);
        let height = (self.canvas.height.as_f32() - 2.0 * FIT_MARGIN).max(1.0);
        (width / self.diagram.width)
            .min(height / self.diagram.height)
            .clamp(MIN_ZOOM, MAX_FIT_ZOOM)
    }

    /// Centers the diagram at `zoom`.
    fn center(&mut self, zoom: f32) {
        self.zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.offset = point(
            (self.canvas.width - px(self.diagram.width * self.zoom)) / 2.0,
            (self.canvas.height - px(self.diagram.height * self.zoom)) / 2.0,
        );
    }

    /// Zooms to `zoom`, keeping the point under `anchor` (canvas
    /// coordinates) in place.
    fn zoom_at(&mut self, zoom: f32, anchor: Point<Pixels>, cx: &mut Context<Self>) {
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let ratio = zoom / self.zoom;
        self.offset = point(
            anchor.x - (anchor.x - self.offset.x) * ratio,
            anchor.y - (anchor.y - self.offset.y) * ratio,
        );
        self.zoom = zoom;
        self.fitted = false;
        cx.notify();
    }

    /// Zooms by `factor` around the middle of the canvas.
    fn zoom_by(&mut self, factor: f32, cx: &mut Context<Self>) {
        let middle = point(self.canvas.width / 2.0, self.canvas.height / 2.0);
        self.zoom_at(self.zoom * factor, middle, cx);
    }

    fn fit(&mut self, cx: &mut Context<Self>) {
        self.fitted = true;
        cx.notify();
    }

    fn actual_size(&mut self, cx: &mut Context<Self>) {
        self.fitted = false;
        self.center(1.0);
        cx.notify();
    }

    /// A window position in canvas coordinates.
    fn to_canvas(position: Point<Pixels>) -> Point<Pixels> {
        point(position.x, position.y - TOOLBAR_HEIGHT)
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(px(20.0));
        if event.modifiers.platform || event.modifiers.control {
            let factor = 2f32.powf(delta.y.as_f32() / SCROLL_PIXELS_PER_DOUBLING);
            self.zoom_at(self.zoom * factor, Self::to_canvas(event.position), cx);
        } else {
            self.offset += delta;
            self.fitted = false;
            cx.notify();
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(DiagramViewerEvent::Closed);
    }

    /// The diagram at its zoom and offset, with a sharper copy over it when
    /// the zoom needs more pixels than its natural size has on a display of
    /// `scale_factor`.
    fn render_diagram(&mut self, scale_factor: f32) -> AnyElement {
        let (width, height) = (
            px(self.diagram.width * self.zoom),
            px(self.diagram.height * self.zoom),
        );
        let place = |image: Arc<Image>| {
            img(image)
                .absolute()
                .left(self.offset.x)
                .top(self.offset.y)
                .w(width)
                .h(height)
                .object_fit(ObjectFit::Fill)
        };
        let scale = self.diagram.sharp_scale(self.zoom * scale_factor / 2.0);
        let sharp = (scale > 1).then(|| {
            let diagram = self.diagram.clone();
            let image = self
                .scaled
                .entry(scale)
                .or_insert_with(|| diagram.scaled(scale))
                .clone();
            place(image)
        });
        div()
            .size_full()
            .child(place(self.diagram.image.clone()))
            .children(sharp)
            .into_any_element()
    }
}

impl gpui::Render for DiagramViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        self.canvas = size(
            viewport.width,
            (viewport.height - TOOLBAR_HEIGHT).max(px(1.0)),
        );
        if self.fitted {
            self.center(self.fit_zoom());
        }
        let colors = cx.theme().colors.clone();
        let percent = format!("{:.0}%", self.zoom * 100.0);
        let toolbar = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .h(TOOLBAR_HEIGHT)
            .px_3()
            .border_b_1()
            .border_color(colors.border_variant)
            .bg(colors.title_bar)
            .child(
                icon_button(
                    "diagram-zoom-out",
                    IconName::MagnifyingGlassMinus,
                    "Zoom out (-)",
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.zoom_by(1.0 / ZOOM_STEP, cx))),
            )
            .child(
                div()
                    .w(px(48.0))
                    .flex()
                    .justify_center()
                    .text_sm()
                    .text_color(colors.text_muted)
                    .child(percent),
            )
            .child(
                icon_button(
                    "diagram-zoom-in",
                    IconName::MagnifyingGlassPlus,
                    "Zoom in (=)",
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.zoom_by(ZOOM_STEP, cx))),
            )
            .child(
                button("diagram-fit", "Fit", cx)
                    .on_click(cx.listener(|this, _, _, cx| this.fit(cx)))
                    .child(kbd("0", cx)),
            )
            .child(
                button("diagram-actual", "100%", cx)
                    .on_click(cx.listener(|this, _, _, cx| this.actual_size(cx)))
                    .child(kbd("1", cx)),
            )
            .child(div().flex_1())
            .child(
                kbd("Esc", cx)
                    .id("diagram-close")
                    .cursor_pointer()
                    .hover(|style| style.bg(colors.element_hover))
                    .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
            );
        let dragging = self.drag.is_some();
        let canvas = div()
            .id("diagram-canvas")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .cursor(if dragging {
                CursorStyle::ClosedHand
            } else {
                CursorStyle::OpenHand
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                    this.drag = Some(Drag {
                        pointer: event.position,
                        offset: this.offset,
                    });
                    this.fitted = false;
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                let Some(drag) = this.drag else {
                    return;
                };
                if event.pressed_button == Some(MouseButton::Left) {
                    this.offset = drag.offset + (event.position - drag.pointer);
                } else {
                    this.drag = None;
                }
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.drag = None;
                    cx.notify();
                }),
            )
            .on_scroll_wheel(
                cx.listener(|this, event: &ScrollWheelEvent, _, cx| this.scroll(event, cx)),
            )
            .on_pinch(cx.listener(|this, event: &gpui::PinchEvent, _, cx| {
                let anchor = Self::to_canvas(event.position);
                this.zoom_at(this.zoom * (1.0 + event.delta), anchor, cx);
            }))
            .child(self.render_diagram(window.scale_factor()));
        div()
            .id("diagram-viewer")
            .key_context(DIAGRAM_VIEWER_CONTEXT)
            .track_focus(&self.focus_handle)
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .bg(colors.background)
            .occlude()
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom_by(ZOOM_STEP, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom_by(1.0 / ZOOM_STEP, cx)))
            .on_action(cx.listener(|this, _: &ZoomToFit, _, cx| this.fit(cx)))
            .on_action(cx.listener(|this, _: &ZoomToActualSize, _, cx| this.actual_size(cx)))
            .child(toolbar)
            .child(canvas)
    }
}
