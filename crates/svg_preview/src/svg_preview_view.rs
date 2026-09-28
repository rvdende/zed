use std::mem;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use file_icons::FileIcons;
use gpui::{
    App, Bounds, Context, DevicePixels, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, ParsedSvg,
    PinchEvent, Pixels, Point, Render, RenderImage, ScrollDelta, ScrollWheelEvent, Styled,
    Subscription, SvgSize, Task, WeakEntity, Window, canvas, div, img, point, px, size,
};
use image_viewer::{FitToView, ResetZoom, ZoomIn, ZoomOut, ZoomToActualSize};
use language::{Buffer, BufferEvent};
use multi_buffer::MultiBuffer;
use project::ProjectPath;
use ui::{Tooltip, prelude::*};
use workspace::item::{Item, ItemHandle};
use workspace::{Pane, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace};

use crate::{OpenFollowingPreview, OpenPreview, OpenPreviewToTheSide};

const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 20.0;
const ZOOM_STEP: f32 = 1.1;
const SCROLL_LINE_MULTIPLIER: f32 = 20.0;
// Rasterizing the whole document at high zoom levels gets expensive quickly, so past this
// size the raster is scaled up on the GPU instead of being re-rendered at full resolution.
const MAX_RASTER_DIMENSION: f32 = 4096.0;
const RASTERIZE_DEBOUNCE: Duration = Duration::from_millis(100);

pub struct SvgPreviewView {
    focus_handle: FocusHandle,
    buffer: Option<Entity<Buffer>>,
    parsed_svg: Option<Arc<ParsedSvg>>,
    current_svg: Option<Result<Arc<RenderImage>, SharedString>>,
    rasterized_width: Option<i32>,
    pending_raster_width: Option<i32>,
    /// `None` means the SVG is fitted to the view (never scaling beyond 100%).
    zoom_level: Option<f32>,
    pan_offset: Point<Pixels>,
    last_mouse_position: Option<Point<Pixels>>,
    container_bounds: Option<Bounds<Pixels>>,
    _parse: Task<()>,
    _rasterize: Task<()>,
    _buffer_subscription: Option<Subscription>,
    _workspace_subscription: Option<Subscription>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SvgPreviewMode {
    /// The preview will always show the contents of the provided editor.
    Default,
    /// The preview will "follow" the last active editor of an SVG file.
    Follow,
}

impl SvgPreviewView {
    pub fn new(
        mode: SvgPreviewMode,
        buffer: Option<Entity<Buffer>>,
        workspace_handle: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let workspace_subscription = if mode == SvgPreviewMode::Follow
                && let Some(workspace) = workspace_handle.upgrade()
            {
                Some(Self::subscribe_to_workspace(workspace, window, cx))
            } else {
                None
            };

            let subscription = buffer
                .as_ref()
                .map(|buffer| Self::create_buffer_subscription(buffer, window, cx));

            let mut this = Self {
                focus_handle: cx.focus_handle(),
                buffer,
                parsed_svg: None,
                current_svg: None,
                rasterized_width: None,
                pending_raster_width: None,
                zoom_level: None,
                pan_offset: Point::default(),
                last_mouse_position: None,
                container_bounds: None,
                _parse: Task::ready(()),
                _rasterize: Task::ready(()),
                _buffer_subscription: subscription,
                _workspace_subscription: workspace_subscription,
            };
            this.parse_buffer(window, cx);

            this
        })
    }

    fn subscribe_to_workspace(
        workspace: Entity<Workspace>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(
            &workspace,
            window,
            move |this: &mut SvgPreviewView, workspace, event: &workspace::Event, window, cx| {
                if let workspace::Event::ActiveItemChanged = event {
                    let workspace = workspace.read(cx);
                    if let Some(active_item) = workspace.active_item(cx)
                        && let Some(buffer) = active_item.downcast::<MultiBuffer>()
                        && Self::is_svg_file(&buffer, cx)
                    {
                        let Some(buffer) = buffer.read(cx).as_singleton() else {
                            return;
                        };
                        if this.buffer.as_ref() != Some(&buffer) {
                            this._buffer_subscription =
                                Some(Self::create_buffer_subscription(&buffer, window, cx));
                            this.buffer = Some(buffer);
                            this.zoom_level = None;
                            this.pan_offset = Point::default();
                            this.parse_buffer(window, cx);
                            cx.notify();
                        }
                    } else {
                        this.buffer = None;
                        this._buffer_subscription = None;
                        this.parsed_svg = None;
                        this.set_current(None, window, cx);
                    }
                }
            },
        )
    }

    fn parse_buffer(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(buffer) = self.buffer.as_ref() else {
            return;
        };

        let renderer = cx.svg_renderer();
        let content = buffer.read(cx).snapshot();
        let background_task =
            cx.background_spawn(async move { renderer.parse_svg(content.text().as_bytes()) });

        self._parse = cx.spawn_in(window, async move |this, cx| {
            let result = background_task.await;

            this.update_in(cx, |this, window, cx| match result {
                Ok(parsed_svg) => {
                    this.parsed_svg = Some(Arc::new(parsed_svg));
                    this.rasterized_width = None;
                    this.pending_raster_width = None;
                    this.update_raster(window, cx);
                    cx.notify();
                }
                Err(error) => {
                    this.parsed_svg = None;
                    this.set_current(Some(Err(error.to_string().into())), window, cx);
                }
            })
            .ok();
        });
    }

    /// Re-rasterizes the SVG when the displayed size no longer matches the raster, so that
    /// zooming in stays sharp instead of scaling up a low resolution bitmap.
    fn update_raster(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(parsed_svg) = self.parsed_svg.clone() else {
            return;
        };
        // Wait for the first layout so a fitted SVG isn't rasterized at the wrong size.
        if self.zoom_level.is_none() && self.container_bounds.is_none() {
            return;
        }

        let svg_size = parsed_svg.size();
        let longest_side = svg_size.width.max(svg_size.height);
        if longest_side <= 0.0 {
            return;
        }
        let scale = (self.effective_zoom() * window.scale_factor())
            .min(MAX_RASTER_DIMENSION / longest_side);
        let target_width = (svg_size.width * scale).round().max(1.0) as i32;
        if self.rasterized_width == Some(target_width)
            || self.pending_raster_width == Some(target_width)
        {
            return;
        }

        self.pending_raster_width = Some(target_width);
        let debounce = self.rasterized_width.is_some();
        let renderer = cx.svg_renderer();
        self._rasterize = cx.spawn_in(window, async move |this, cx| {
            if debounce {
                cx.background_executor().timer(RASTERIZE_DEBOUNCE).await;
            }
            let result = cx
                .background_spawn(async move {
                    let target_size = size(DevicePixels(target_width), DevicePixels(target_width));
                    renderer.render_parsed(&parsed_svg, SvgSize::Size(target_size))
                })
                .await;

            this.update_in(cx, |this, window, cx| {
                this.pending_raster_width = None;
                this.rasterized_width = Some(target_width);
                let current = result.map_err(|error| error.to_string().into());
                this.set_current(Some(current), window, cx);
            })
            .ok();
        });
    }

    fn set_current(
        &mut self,
        image: Option<Result<Arc<RenderImage>, SharedString>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if image.is_none() {
            self.rasterized_width = None;
            self.pending_raster_width = None;
            self._rasterize = Task::ready(());
        }
        if let Some(Ok(image)) = mem::replace(&mut self.current_svg, image) {
            window.drop_image(image).ok();
        }
        cx.notify();
    }

    fn fit_to_view_zoom(&self) -> Option<f32> {
        let bounds = self.container_bounds?;
        let svg_size = self.parsed_svg.as_ref()?.size();
        if svg_size.width <= 0.0 || svg_size.height <= 0.0 {
            return None;
        }
        let scale_x = f32::from(bounds.size.width) / svg_size.width;
        let scale_y = f32::from(bounds.size.height) / svg_size.height;
        Some(scale_x.min(scale_y).min(1.0))
    }

    pub fn effective_zoom(&self) -> f32 {
        self.zoom_level
            .or_else(|| self.fit_to_view_zoom())
            .unwrap_or(1.0)
    }

    fn set_zoom(
        &mut self,
        new_zoom: f32,
        zoom_center: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let old_zoom = self.effective_zoom();
        let new_zoom = new_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.zoom_level = Some(new_zoom);

        if let Some((center, bounds)) = zoom_center.zip(self.container_bounds) {
            let relative_center = point(
                center.x - bounds.origin.x - bounds.size.width / 2.0,
                center.y - bounds.origin.y - bounds.size.height / 2.0,
            );
            let mouse_offset_from_image = relative_center - self.pan_offset;
            let zoom_ratio = new_zoom / old_zoom;
            self.pan_offset += mouse_offset_from_image * (1.0 - zoom_ratio);
        }

        cx.notify();
    }

    fn zoom_in(&mut self, _: &ZoomIn, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(self.effective_zoom() * ZOOM_STEP, None, cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(self.effective_zoom() / ZOOM_STEP, None, cx);
    }

    fn reset_zoom(&mut self, _: &ResetZoom, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_level = Some(1.0);
        self.pan_offset = Point::default();
        cx.notify();
    }

    fn zoom_to_actual_size(
        &mut self,
        _: &ZoomToActualSize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reset_zoom(&ResetZoom, window, cx);
    }

    fn fit_to_view(&mut self, _: &FitToView, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_level = None;
        self.pan_offset = Point::default();
        cx.notify();
    }

    fn handle_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.modifiers.control || event.modifiers.platform {
            let delta: f32 = match event.delta {
                ScrollDelta::Pixels(pixels) => pixels.y.into(),
                ScrollDelta::Lines(lines) => lines.y * SCROLL_LINE_MULTIPLIER,
            };
            let zoom_factor = if delta > 0.0 {
                1.0 + delta.abs() * 0.01
            } else {
                1.0 / (1.0 + delta.abs() * 0.01)
            };
            self.set_zoom(
                self.effective_zoom() * zoom_factor,
                Some(event.position),
                cx,
            );
        } else {
            let delta = match event.delta {
                ScrollDelta::Pixels(pixels) => pixels,
                ScrollDelta::Lines(lines) => lines.map(|line| px(line * SCROLL_LINE_MULTIPLIER)),
            };
            self.zoom_level = Some(self.effective_zoom());
            self.pan_offset += delta;
            cx.notify();
        }
    }

    fn handle_pinch(&mut self, event: &PinchEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let zoom_factor = 1.0 + event.delta;
        self.set_zoom(
            self.effective_zoom() * zoom_factor,
            Some(event.position),
            cx,
        );
    }

    fn handle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.last_mouse_position = Some(event.position);
        cx.notify();
    }

    fn handle_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.last_mouse_position = None;
        cx.notify();
    }

    fn handle_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(last_position) = self.last_mouse_position {
            self.zoom_level = Some(self.effective_zoom());
            self.pan_offset += event.position - last_position;
            self.last_mouse_position = Some(event.position);
            cx.notify();
        }
    }

    fn find_existing_preview_item_idx(
        pane: &Pane,
        buffer: &Entity<Buffer>,
        cx: &App,
    ) -> Option<usize> {
        pane.items_of_type::<SvgPreviewView>()
            .find(|view| view.read(cx).buffer.as_ref() == Some(buffer))
            .and_then(|view| pane.index_for_item(&view))
    }

    pub fn resolve_active_item_as_svg_buffer(
        workspace: &Workspace,
        cx: &mut Context<Workspace>,
    ) -> Option<Entity<MultiBuffer>> {
        workspace
            .active_item(cx)?
            .act_as::<MultiBuffer>(cx)
            .filter(|buffer| Self::is_svg_file(&buffer, cx))
    }

    fn create_buffer_subscription(
        buffer: &Entity<Buffer>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(
            buffer,
            window,
            move |this, _buffer, event: &BufferEvent, window, cx| match event {
                BufferEvent::Edited { .. } | BufferEvent::Saved => {
                    this.parse_buffer(window, cx);
                }
                _ => {}
            },
        )
    }

    pub fn is_svg_file(buffer: &Entity<MultiBuffer>, cx: &App) -> bool {
        buffer
            .read(cx)
            .as_singleton()
            .and_then(|buffer| buffer.read(cx).file())
            .is_some_and(|file| Self::is_svg_path(file.file_name(cx)))
    }

    pub fn is_svg_path(path: impl AsRef<std::path::Path>) -> bool {
        path.as_ref()
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"))
    }

    pub fn open_preview_in_pane(
        workspace: &mut Workspace,
        buffer: Entity<MultiBuffer>,
        pane: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(buffer) = buffer.read(cx).as_singleton() else {
            return;
        };
        Self::activate_or_add_preview(workspace, buffer, pane, true, false, window, cx);
    }

    pub fn open_preview_to_the_side_of_pane(
        workspace: &mut Workspace,
        buffer: Entity<MultiBuffer>,
        origin_pane: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(buffer) = buffer.read(cx).as_singleton() else {
            return;
        };
        let target_pane = workspace.adjacent_pane_of(&origin_pane, window, cx);
        Self::activate_or_add_preview(workspace, buffer, target_pane, false, false, window, cx);
    }

    /// Opens a preview of the SVG file at `project_path` without opening it in an editor.
    pub fn open_for_project_path(
        project_path: ProjectPath,
        focus: bool,
        allow_preview: bool,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Task<Result<()>> {
        let open_buffer = workspace
            .project()
            .update(cx, |project, cx| project.open_buffer(project_path, cx));

        cx.spawn_in(window, async move |workspace, cx| {
            let buffer = open_buffer.await?;
            workspace.update_in(cx, |workspace, window, cx| {
                let pane = workspace.active_pane().clone();
                Self::activate_or_add_preview(
                    workspace,
                    buffer,
                    pane,
                    focus,
                    allow_preview,
                    window,
                    cx,
                );
            })
        })
    }

    fn activate_or_add_preview(
        workspace: &mut Workspace,
        buffer: Entity<Buffer>,
        pane: Entity<Pane>,
        focus: bool,
        allow_preview: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let existing_view_idx = Self::find_existing_preview_item_idx(pane.read(cx), &buffer, cx);
        if let Some(existing_view_idx) = existing_view_idx {
            pane.update(cx, |pane, cx| {
                if !allow_preview && let Some(item) = pane.item_for_index(existing_view_idx) {
                    pane.unpreview_item_if_preview(item.item_id());
                }
                pane.activate_item(existing_view_idx, focus, focus, window, cx);
            });
        } else {
            let view = SvgPreviewView::new(
                SvgPreviewMode::Default,
                Some(buffer),
                workspace.weak_handle(),
                window,
                cx,
            );
            pane.update(cx, |pane, cx| {
                let destination_index = if allow_preview {
                    pane.replace_preview_item_id(view.item_id(), window, cx)
                } else {
                    None
                };
                pane.add_item(Box::new(view), focus, focus, destination_index, window, cx)
            });
        }
        cx.notify();
    }

    pub fn register(workspace: &mut Workspace, _window: &mut Window, _cx: &mut Context<Workspace>) {
        workspace.register_action(move |workspace, _: &OpenPreview, window, cx| {
            if let Some(buffer) = Self::resolve_active_item_as_svg_buffer(workspace, cx) {
                let pane = workspace.active_pane().clone();
                Self::open_preview_in_pane(workspace, buffer, pane, window, cx);
            }
        });

        workspace.register_action(move |workspace, _: &OpenPreviewToTheSide, window, cx| {
            if let Some(buffer) = Self::resolve_active_item_as_svg_buffer(workspace, cx) {
                let pane = workspace.active_pane().clone();
                Self::open_preview_to_the_side_of_pane(workspace, buffer, pane, window, cx);
            }
        });

        workspace.register_action(move |workspace, _: &OpenFollowingPreview, window, cx| {
            if let Some(buffer) = Self::resolve_active_item_as_svg_buffer(workspace, cx) {
                let buffer = buffer.read(cx).as_singleton();
                let view = SvgPreviewView::new(
                    SvgPreviewMode::Follow,
                    buffer,
                    workspace.weak_handle(),
                    window,
                    cx,
                );
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.add_item(Box::new(view), true, true, None, window, cx)
                });
                cx.notify();
            }
        });
    }
}

impl Render for SvgPreviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_raster(window, cx);

        let zoom = self.effective_zoom();
        let svg_size = self.parsed_svg.as_ref().map(|svg| svg.size());
        let is_dragging = self.last_mouse_position.is_some();

        div()
            .id("SvgPreview")
            .key_context("SvgPreview")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::reset_zoom))
            .on_action(cx.listener(Self::zoom_to_actual_size))
            .on_action(cx.listener(Self::fit_to_view))
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(cx.theme().colors().editor_background)
            .on_scroll_wheel(cx.listener(Self::handle_scroll_wheel))
            .on_pinch(cx.listener(Self::handle_pinch))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::handle_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::handle_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::handle_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::handle_mouse_up))
            .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::handle_mouse_up))
            .on_mouse_move(cx.listener(Self::handle_mouse_move))
            .child(
                canvas(
                    {
                        let this = cx.entity().downgrade();
                        move |bounds, _window, cx| {
                            this.update(cx, |this, cx| {
                                if this.container_bounds != Some(bounds) {
                                    this.container_bounds = Some(bounds);
                                    cx.notify();
                                }
                            })
                            .ok();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .map(|this| match (self.current_svg.clone(), svg_size) {
                (Some(Ok(image)), Some(svg_size)) => {
                    let Some(bounds) = self.container_bounds else {
                        return this;
                    };
                    let width = px(svg_size.width * zoom);
                    let height = px(svg_size.height * zoom);
                    let left = bounds.size.width / 2.0 - width / 2.0 + self.pan_offset.x;
                    let top = bounds.size.height / 2.0 - height / 2.0 + self.pan_offset.y;
                    this.cursor(if is_dragging {
                        gpui::CursorStyle::ClosedHand
                    } else {
                        gpui::CursorStyle::OpenHand
                    })
                    .child(
                        div()
                            .absolute()
                            .left(left)
                            .top(top)
                            .w(width)
                            .h(height)
                            .child(img(image).size_full().with_fallback(|| {
                                h_flex()
                                    .p_4()
                                    .gap_2()
                                    .child(Icon::new(IconName::Warning))
                                    .child("Failed to load SVG image")
                                    .into_any_element()
                            })),
                    )
                }
                (Some(Err(error)), _) => this.child(
                    h_flex()
                        .size_full()
                        .justify_center()
                        .child(div().p_4().child(error)),
                ),
                (None, _) if self.buffer.is_none() => this.child(
                    h_flex()
                        .size_full()
                        .justify_center()
                        .child(div().p_4().child("No SVG file selected")),
                ),
                _ => this,
            })
    }
}

impl Focusable for SvgPreviewView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for SvgPreviewView {}

impl Item for SvgPreviewView {
    type Event = ();

    fn tab_icon(&self, _window: &Window, cx: &App) -> Option<Icon> {
        self.buffer
            .as_ref()
            .and_then(|buffer| buffer.read(cx).file())
            .and_then(|file| FileIcons::get_icon(file.path().as_std_path(), cx))
            .map(Icon::from_path)
            .or_else(|| Some(Icon::new(IconName::Image)))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.buffer
            .as_ref()
            .and_then(|svg_path| svg_path.read(cx).file())
            .map(|name| format!("Preview {}", name.file_name(cx)).into())
            .unwrap_or_else(|| "SVG Preview".into())
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("svg preview: open")
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(workspace::item::ItemEvent)) {}
}

pub struct SvgPreviewToolbarControls {
    preview: Option<WeakEntity<SvgPreviewView>>,
    _subscription: Option<Subscription>,
}

impl SvgPreviewToolbarControls {
    pub fn new() -> Self {
        Self {
            preview: None,
            _subscription: None,
        }
    }
}

impl Render for SvgPreviewToolbarControls {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(preview) = self.preview.as_ref().and_then(|preview| preview.upgrade()) else {
            return div().into_any_element();
        };

        let zoom_percentage = format!("{}%", (preview.read(cx).effective_zoom() * 100.0).round());
        let update_preview =
            move |update: fn(&mut SvgPreviewView, &mut Window, &mut Context<SvgPreviewView>)| {
                let preview = preview.downgrade();
                move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                    preview
                        .update(cx, |preview, cx| update(preview, window, cx))
                        .ok();
                }
            };

        h_flex()
            .gap_1()
            .child(
                IconButton::new("svg-zoom-out", IconName::Dash)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| Tooltip::for_action("Zoom Out", &ZoomOut, cx))
                    .on_click(update_preview(|preview, window, cx| {
                        preview.zoom_out(&ZoomOut, window, cx)
                    })),
            )
            .child(
                Button::new("svg-reset-zoom", zoom_percentage)
                    .label_size(LabelSize::Small)
                    .tooltip(|_window, cx| Tooltip::for_action("Reset Zoom", &ResetZoom, cx))
                    .on_click(update_preview(|preview, window, cx| {
                        preview.reset_zoom(&ResetZoom, window, cx)
                    })),
            )
            .child(
                IconButton::new("svg-zoom-in", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| Tooltip::for_action("Zoom In", &ZoomIn, cx))
                    .on_click(update_preview(|preview, window, cx| {
                        preview.zoom_in(&ZoomIn, window, cx)
                    })),
            )
            .child(
                IconButton::new("svg-fit-to-view", IconName::Maximize)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| Tooltip::for_action("Fit to View", &FitToView, cx))
                    .on_click(update_preview(|preview, window, cx| {
                        preview.fit_to_view(&FitToView, window, cx)
                    })),
            )
            .into_any_element()
    }
}

impl EventEmitter<ToolbarItemEvent> for SvgPreviewToolbarControls {}

impl ToolbarItemView for SvgPreviewToolbarControls {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.preview = None;
        self._subscription = None;

        if let Some(preview) = active_pane_item.and_then(|item| item.downcast::<SvgPreviewView>()) {
            self._subscription = Some(cx.observe(&preview, |_, _, cx| cx.notify()));
            self.preview = Some(preview.downgrade());
            cx.notify();
            return ToolbarItemLocation::PrimaryRight;
        }

        ToolbarItemLocation::Hidden
    }
}
