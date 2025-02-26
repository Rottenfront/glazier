// Copyright 2020 The Druid Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Web window creation and management.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};

use instant::Instant;
use tracing::{error, warn};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use raw_window_handle::{HasRawWindowHandle, RawWindowHandle, WebWindowHandle};

use crate::kurbo::{Insets, Point, Rect, Size, Vec2};

use crate::piet::{PietText, RenderContext};

use super::application::Application;
use super::error::Error;
use super::keycodes::convert_keyboard_event;
use super::menu::Menu;
use crate::common_util::{ClickCounter, IdleCallback};
use crate::dialog::{FileDialogOptions, FileDialogType};
use crate::error::Error as ShellError;
use crate::scale::{Scale, ScaledArea};

use crate::keyboard::{KeyState, Modifiers};
use crate::mouse::{Cursor, CursorDesc};
use crate::pointer::{
    MouseInfo, PointerButton, PointerButtons, PointerEvent, PointerId, PointerType,
};
use crate::region::Region;
use crate::text::{simulate_input, Event};
use crate::window;
use crate::window::{
    FileDialogToken, IdleToken, TextFieldToken, TimerToken, WinHandler, WindowLevel,
};

// This is a macro instead of a function since KeyboardEvent and MouseEvent has identical functions
// to query modifier key states.
macro_rules! get_modifiers {
    ($event:ident) => {{
        let mut result = Modifiers::default();
        result.set(Modifiers::SHIFT, $event.shift_key());
        result.set(Modifiers::ALT, $event.alt_key());
        result.set(Modifiers::CONTROL, $event.ctrl_key());
        result.set(Modifiers::META, $event.meta_key());
        result.set(Modifiers::ALT_GRAPH, $event.get_modifier_state("AltGraph"));
        result.set(Modifiers::CAPS_LOCK, $event.get_modifier_state("CapsLock"));
        result.set(Modifiers::NUM_LOCK, $event.get_modifier_state("NumLock"));
        result.set(
            Modifiers::SCROLL_LOCK,
            $event.get_modifier_state("ScrollLock"),
        );
        result
    }};
}

/// Builder abstraction for creating new windows.
pub(crate) struct WindowBuilder {
    handler: Option<Box<dyn WinHandler>>,
    title: String,
    cursor: Cursor,
    menu: Option<Menu>,
}

#[derive(Clone, Default)]
pub struct WindowHandle(Weak<WindowState>);
impl PartialEq for WindowHandle {
    fn eq(&self, other: &Self) -> bool {
        match (self.0.upgrade(), other.0.upgrade()) {
            (None, None) => true,
            (Some(s), Some(o)) => std::rc::Rc::ptr_eq(&s, &o),
            (_, _) => false,
        }
    }
}
impl Eq for WindowHandle {}

unsafe impl HasRawWindowHandle for WindowHandle {
    fn raw_window_handle(&self) -> RawWindowHandle {
        error!("HasRawWindowHandle trait not implemented for wasm.");
        RawWindowHandle::Web(WebWindowHandle::empty())
    }
}

/// A handle that can get used to schedule an idle handler. Note that
/// this handle is thread safe.
#[derive(Clone)]
pub struct IdleHandle {
    state: Weak<WindowState>,
    queue: Arc<Mutex<Vec<IdleKind>>>,
}

enum IdleKind {
    Callback(IdleCallback),
    Token(IdleToken),
}

struct WindowState {
    scale: Cell<Scale>,
    area: Cell<ScaledArea>,
    idle_queue: Arc<Mutex<Vec<IdleKind>>>,
    handler: RefCell<Box<dyn WinHandler>>,
    window: web_sys::Window,
    canvas: web_sys::HtmlCanvasElement,
    canvas_size: Option<Size>,
    context: web_sys::CanvasRenderingContext2d,
    invalid: RefCell<Region>,
    click_counter: ClickCounter,
    active_text_input: Cell<Option<TextFieldToken>>,
    rendering_soon: Cell<bool>,
}

// TODO: support custom cursors
#[derive(Clone, PartialEq, Eq)]
pub struct CustomCursor;

impl WindowState {
    fn render(&self) {
        self.handler.borrow_mut().prepare_paint();

        let mut piet_ctx = piet_common::Piet::new(self.context.clone(), self.window.clone());
        if let Err(e) = piet_ctx.with_save(|ctx| {
            let invalid = self.invalid.borrow();
            ctx.clip(invalid.to_bez_path());
            self.handler.borrow_mut().paint(ctx, &invalid);
            Ok(())
        }) {
            error!("piet error on render: {:?}", e);
        }
        if let Err(e) = piet_ctx.finish() {
            error!("piet error finishing render: {:?}", e);
        }
        self.invalid.borrow_mut().clear();
    }

    fn process_idle_queue(&self) {
        let mut queue = self.idle_queue.lock().expect("process_idle_queue");
        for item in queue.drain(..) {
            match item {
                IdleKind::Callback(cb) => cb(&mut **self.handler.borrow_mut()),
                IdleKind::Token(tok) => self.handler.borrow_mut().idle(tok),
            }
        }
    }

    fn request_animation_frame(&self, f: impl FnOnce() + 'static) -> Result<i32, Error> {
        Ok(self
            .window
            .request_animation_frame(Closure::once_into_js(f).as_ref().unchecked_ref())?)
    }

    /// Returns the window size in css units
    fn get_window_size_and_dpr(&self) -> (f64, f64, f64) {
        let w = &self.window;
        let dpr = w.device_pixel_ratio();

        match self.canvas_size {
            Some(Size { width, height }) => (width, height, dpr),
            _ => {
                let width = w.inner_width().unwrap().as_f64().unwrap();
                let height = w.inner_height().unwrap().as_f64().unwrap();
                (width, height, dpr)
            }
        }
    }

    /// Updates the canvas size and scale factor and returns `Scale` and `ScaledArea`.
    fn update_scale_and_area(&self) -> (Scale, ScaledArea) {
        let (css_width, css_height, dpr) = self.get_window_size_and_dpr();
        let scale = Scale::new(dpr, dpr);
        let area = ScaledArea::from_dp(Size::new(css_width, css_height), scale);
        let size_px = area.size_px();
        self.canvas.set_width(size_px.width as u32);
        self.canvas.set_height(size_px.height as u32);
        let _ = self.context.scale(scale.x(), scale.y());
        self.scale.set(scale);
        self.area.set(area);
        (scale, area)
    }
}

fn setup_mouse_down_callback(ws: &Rc<WindowState>) {
    let state = ws.clone();
    register_canvas_event_listener(ws, "mousedown", move |event: web_sys::MouseEvent| {
        if let Some(button) = get_button(event.button()) {
            let pos = Point::new(event.offset_x() as f64, event.offset_y() as f64);
            let count = state.click_counter.count_for_click(pos);
            let event = PointerEvent {
                pointer_id: PointerId(0),
                is_primary: true,
                pointer_type: PointerType::Mouse(MouseInfo {
                    wheel_delta: Vec2::ZERO,
                }),
                pos,
                buttons: get_buttons(event.buttons()),
                modifiers: get_modifiers!(event),
                button,
                focus: false,
                count,
            };
            state.handler.borrow_mut().pointer_down(&event);
        }
    });
}

fn setup_mouse_up_callback(ws: &Rc<WindowState>) {
    let state = ws.clone();
    register_canvas_event_listener(ws, "mouseup", move |event: web_sys::MouseEvent| {
        if let Some(button) = get_button(event.button()) {
            let event = PointerEvent {
                pointer_id: PointerId(0),
                is_primary: true,
                pointer_type: PointerType::Mouse(MouseInfo {
                    wheel_delta: Vec2::ZERO,
                }),
                pos: Point::new(event.offset_x() as f64, event.offset_y() as f64),
                buttons: get_buttons(event.buttons()),
                modifiers: get_modifiers!(event),
                button,
                focus: false,
                count: 0,
            };
            state.handler.borrow_mut().pointer_up(&event);
        }
    });
}

fn setup_mouse_move_callback(ws: &Rc<WindowState>) {
    let state = ws.clone();
    register_canvas_event_listener(ws, "mousemove", move |event: web_sys::MouseEvent| {
        let event = PointerEvent {
            pointer_id: PointerId(0),
            is_primary: true,
            pointer_type: PointerType::Mouse(MouseInfo {
                wheel_delta: Vec2::ZERO,
            }),
            pos: Point::new(event.offset_x() as f64, event.offset_y() as f64),
            buttons: get_buttons(event.buttons()),
            modifiers: get_modifiers!(event),
            button: PointerButton::None,
            focus: false,
            count: 0,
        };
        state.handler.borrow_mut().pointer_move(&event);
    });
}

fn setup_scroll_callback(ws: &Rc<WindowState>) {
    let state = ws.clone();
    register_canvas_event_listener(ws, "wheel", move |event: web_sys::WheelEvent| {
        let delta_mode = event.delta_mode();

        let dx = event.delta_x();
        let dy = event.delta_y();

        // The value 35.0 was manually picked to produce similar behavior to mac/linux.
        let wheel_delta = match delta_mode {
            web_sys::WheelEvent::DOM_DELTA_PIXEL => Vec2::new(dx, dy),
            web_sys::WheelEvent::DOM_DELTA_LINE => Vec2::new(35.0 * dx, 35.0 * dy),
            web_sys::WheelEvent::DOM_DELTA_PAGE => {
                let size_dp = state.area.get().size_dp();
                Vec2::new(size_dp.width * dx, size_dp.height * dy)
            }
            _ => {
                warn!("Invalid deltaMode in WheelEvent: {}", delta_mode);
                return;
            }
        };

        let event = PointerEvent {
            pointer_id: PointerId(0),
            is_primary: true,
            pointer_type: PointerType::Mouse(MouseInfo { wheel_delta }),
            pos: Point::new(event.offset_x() as f64, event.offset_y() as f64),
            buttons: get_buttons(event.buttons()),
            modifiers: get_modifiers!(event),
            button: PointerButton::None,
            focus: false,
            count: 0,
        };
        state.handler.borrow_mut().wheel(&event);
    });
}

fn setup_resize_callback(ws: &Rc<WindowState>) {
    let state = ws.clone();
    register_window_event_listener(ws, "resize", move |_: web_sys::UiEvent| {
        let (scale, area) = state.update_scale_and_area();
        // TODO: For performance, only call the handler when these values actually changed.
        state.handler.borrow_mut().scale(scale);
        state.handler.borrow_mut().size(area.size_dp());
    });
}

fn setup_keyup_callback(ws: &Rc<WindowState>) {
    let state = ws.clone();
    register_window_event_listener(ws, "keyup", move |event: web_sys::KeyboardEvent| {
        let modifiers = get_modifiers!(event);
        let kb_event = convert_keyboard_event(&event, modifiers, KeyState::Up);
        state.handler.borrow_mut().key_up(&kb_event);
    });
}

fn setup_keydown_callback(ws: &Rc<WindowState>) {
    let state = ws.clone();
    register_window_event_listener(ws, "keydown", move |event: web_sys::KeyboardEvent| {
        let modifiers = get_modifiers!(event);
        let kb_event = convert_keyboard_event(&event, modifiers, KeyState::Down);
        let mut handler = state.handler.borrow_mut();
        if simulate_input(&mut **handler, state.active_text_input.get(), kb_event) {
            event.prevent_default();
        }
    });
}

/// A helper function to register a window event listener with `addEventListener`.
fn register_window_event_listener<F, E>(window_state: &Rc<WindowState>, event_type: &str, f: F)
where
    F: 'static + FnMut(E),
    E: 'static + wasm_bindgen::convert::FromWasmAbi,
{
    let closure = Closure::wrap(Box::new(f) as Box<dyn FnMut(_)>);
    window_state
        .window
        .add_event_listener_with_callback(event_type, closure.as_ref().unchecked_ref())
        .unwrap();
    closure.forget();
}

/// A helper function to register a canvas event listener with `addEventListener`.
fn register_canvas_event_listener<F, E>(window_state: &Rc<WindowState>, event_type: &str, f: F)
where
    F: 'static + FnMut(E),
    E: 'static + wasm_bindgen::convert::FromWasmAbi,
{
    let closure = Closure::wrap(Box::new(f) as Box<dyn FnMut(_)>);
    window_state
        .canvas
        .add_event_listener_with_callback(event_type, closure.as_ref().unchecked_ref())
        .unwrap();
    closure.forget();
}

fn setup_web_callbacks(window_state: &Rc<WindowState>) {
    setup_mouse_down_callback(window_state);
    setup_mouse_move_callback(window_state);
    setup_mouse_up_callback(window_state);
    setup_resize_callback(window_state);
    setup_scroll_callback(window_state);
    setup_keyup_callback(window_state);
    setup_keydown_callback(window_state);
}

impl WindowBuilder {
    pub fn new(_app: Application) -> WindowBuilder {
        WindowBuilder {
            handler: None,
            title: String::new(),
            cursor: Cursor::Arrow,
            menu: None,
        }
    }

    /// This takes ownership, and is typically used with UiMain
    pub fn handler(mut self, handler: Box<dyn WinHandler>) -> Self {
        self.handler = Some(handler);
        self
    }

    pub fn size(self, _: Size) -> Self {
        // Ignored
        self
    }

    pub fn min_size(self, _: Size) -> Self {
        // Ignored
        self
    }

    pub fn resizable(self, _resizable: bool) -> Self {
        // Ignored
        self
    }

    pub fn show_titlebar(self, _show_titlebar: bool) -> Self {
        // Ignored
        self
    }

    pub fn transparent(self, _transparent: bool) -> Self {
        // Ignored
        self
    }

    pub fn position(self, _position: Point) -> Self {
        // Ignored
        self
    }

    pub fn window_state(self, _state: window::WindowState) -> Self {
        // Ignored
        self
    }

    pub fn level(self, _level: WindowLevel) -> Self {
        // ignored
        self
    }

    pub fn title<S: Into<String>>(mut self, title: S) -> Self {
        self.title = title.into();
        self
    }

    pub fn menu(mut self, menu: Menu) -> Self {
        self.menu = Some(menu);
        self
    }

    pub fn build(self) -> Result<WindowHandle, Error> {
        let window = web_sys::window().ok_or(Error::NoWindow)?;
        let canvas = window
            .document()
            .ok_or(Error::NoDocument)?
            .get_element_by_id("canvas")
            .ok_or_else(|| Error::NoElementById("canvas".to_string()))?
            .dyn_into::<web_sys::HtmlCanvasElement>()
            .map_err(|_| Error::JsCast)?;

        let cnv_attr = |attr| {
            canvas
                .get_attribute(attr)
                .and_then(|value| value.parse().ok())
        };

        let canvas_size = match (cnv_attr("width"), cnv_attr("height")) {
            (Some(width), Some(height)) => Some(Size::new(width, height)),
            _ => None,
        };

        let context = canvas
            .get_context("2d")?
            .ok_or(Error::NoContext)?
            .dyn_into::<web_sys::CanvasRenderingContext2d>()
            .map_err(|_| Error::JsCast)?;
        // Create the Scale for resolution scaling
        let scale = {
            let dpr = window.device_pixel_ratio();
            Scale::new(dpr, dpr)
        };
        let area = {
            // The initial size in display points isn't necessarily the final size in display points
            let size_dp = Size::new(canvas.offset_width() as f64, canvas.offset_height() as f64);
            ScaledArea::from_dp(size_dp, scale)
        };
        let size_px = area.size_px();
        canvas.set_width(size_px.width as u32);
        canvas.set_height(size_px.height as u32);
        let _ = context.scale(scale.x(), scale.y());
        let size_dp = area.size_dp();

        set_cursor(&canvas, &self.cursor);

        let handler = self.handler.unwrap();

        let window = Rc::new(WindowState {
            scale: Cell::new(scale),
            area: Cell::new(area),
            idle_queue: Default::default(),
            handler: RefCell::new(handler),
            window,
            canvas,
            canvas_size,
            context,
            invalid: RefCell::new(Region::EMPTY),
            click_counter: ClickCounter::default(),
            active_text_input: Cell::new(None),
            rendering_soon: Cell::new(false),
        });

        setup_web_callbacks(&window);

        // Register the scale & size with the window handler.
        let wh = window.clone();
        window
            .request_animation_frame(move || {
                wh.handler.borrow_mut().scale(scale);
                wh.handler.borrow_mut().size(size_dp);
            })
            .expect("Failed to request animation frame");

        let handle = WindowHandle(Rc::downgrade(&window));

        window.handler.borrow_mut().connect(&handle.clone().into());

        Ok(handle)
    }
}

impl WindowHandle {
    pub fn show(&self) {
        self.render_soon();
    }

    pub fn resizable(&self, _resizable: bool) {
        warn!("resizable unimplemented for web");
    }

    pub fn show_titlebar(&self, _show_titlebar: bool) {
        warn!("show_titlebar unimplemented for web");
    }

    pub fn set_position(&self, _position: Point) {
        warn!("WindowHandle::set_position unimplemented for web");
    }

    pub fn get_position(&self) -> Point {
        warn!("WindowHandle::get_position unimplemented for web.");
        Point::new(0.0, 0.0)
    }

    pub fn set_size(&self, _size: Size) {
        warn!("WindowHandle::set_size unimplemented for web.");
    }

    pub fn get_size(&self) -> Size {
        warn!("WindowHandle::get_size unimplemented for web.");
        Size::new(0.0, 0.0)
    }

    pub fn content_insets(&self) -> Insets {
        warn!("WindowHandle::content_insets unimplemented for web.");
        Insets::ZERO
    }

    pub fn set_window_state(&self, _state: window::WindowState) {
        warn!("WindowHandle::set_window_state unimplemented for web.");
    }

    pub fn get_window_state(&self) -> window::WindowState {
        warn!("WindowHandle::get_window_state unimplemented for web.");
        window::WindowState::Restored
    }

    pub fn handle_titlebar(&self, _val: bool) {
        warn!("WindowHandle::handle_titlebar unimplemented for web.");
    }

    pub fn close(&self) {
        // TODO
    }

    pub fn bring_to_front_and_focus(&self) {
        warn!("bring_to_frontand_focus unimplemented for web");
    }

    pub fn request_anim_frame(&self) {
        self.render_soon();
    }

    pub fn invalidate_rect(&self, rect: Rect) {
        if let Some(s) = self.0.upgrade() {
            s.invalid.borrow_mut().add_rect(rect);
        }
        self.render_soon();
    }

    pub fn invalidate(&self) {
        if let Some(s) = self.0.upgrade() {
            s.invalid
                .borrow_mut()
                .add_rect(s.area.get().size_dp().to_rect());
        }
        self.render_soon();
    }

    pub fn text(&self) -> PietText {
        let s = self
            .0
            .upgrade()
            .unwrap_or_else(|| panic!("Failed to produce a text context"));

        PietText::new(s.context.clone())
    }

    pub fn add_text_field(&self) -> TextFieldToken {
        TextFieldToken::next()
    }

    pub fn remove_text_field(&self, token: TextFieldToken) {
        if let Some(state) = self.0.upgrade() {
            if state.active_text_input.get() == Some(token) {
                state.active_text_input.set(None);
            }
        }
    }

    pub fn set_focused_text_field(&self, active_field: Option<TextFieldToken>) {
        if let Some(state) = self.0.upgrade() {
            state.active_text_input.set(active_field);
        }
    }

    pub fn update_text_field(&self, _token: TextFieldToken, _update: Event) {
        // no-op for now, until we get a properly implemented text input
    }

    pub fn request_timer(&self, deadline: Instant) -> TimerToken {
        use std::convert::TryFrom;
        let interval = deadline.duration_since(Instant::now()).as_millis();
        let interval = match i32::try_from(interval) {
            Ok(iv) => iv,
            Err(_) => {
                warn!("Timer duration exceeds 32 bit integer max");
                i32::max_value()
            }
        };

        let token = TimerToken::next();

        if let Some(state) = self.0.upgrade() {
            let s = state.clone();
            let f = move || {
                if let Ok(mut handler_borrow) = s.handler.try_borrow_mut() {
                    handler_borrow.timer(token);
                }
            };
            state
                .window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    Closure::once_into_js(f).as_ref().unchecked_ref(),
                    interval,
                )
                .expect("Failed to call setTimeout with a callback");
        }
        token
    }

    pub fn set_cursor(&mut self, cursor: &Cursor) {
        if let Some(s) = self.0.upgrade() {
            set_cursor(&s.canvas, cursor);
        }
    }

    pub fn make_cursor(&self, _cursor_desc: &CursorDesc) -> Option<Cursor> {
        warn!("Custom cursors are not yet supported in the web backend");
        None
    }

    pub fn open_file(&mut self, _options: FileDialogOptions) -> Option<FileDialogToken> {
        warn!("open_file is currently unimplemented for web.");
        None
    }

    pub fn save_as(&mut self, _options: FileDialogOptions) -> Option<FileDialogToken> {
        warn!("save_as is currently unimplemented for web.");
        None
    }

    fn render_soon(&self) {
        if let Some(s) = self.0.upgrade() {
            let state = s.clone();
            if !state.rendering_soon.get() {
                state.rendering_soon.set(true);
                s.request_animation_frame(move || {
                    state.rendering_soon.set(false);
                    state.render();
                })
                .expect("Failed to request animation frame");
            }
        }
    }

    pub fn file_dialog(
        &self,
        _ty: FileDialogType,
        _options: FileDialogOptions,
    ) -> Result<OsString, ShellError> {
        Err(ShellError::Platform(Error::Unimplemented))
    }

    /// Get a handle that can be used to schedule an idle task.
    pub fn get_idle_handle(&self) -> Option<IdleHandle> {
        self.0.upgrade().map(|w| IdleHandle {
            state: Rc::downgrade(&w),
            queue: w.idle_queue.clone(),
        })
    }

    /// Get the `Scale` of the window.
    pub fn get_scale(&self) -> Result<Scale, ShellError> {
        Ok(self
            .0
            .upgrade()
            .ok_or(ShellError::WindowDropped)?
            .scale
            .get())
    }

    pub fn set_menu(&self, _menu: Menu) {
        warn!("set_menu unimplemented for web");
    }

    pub fn show_context_menu(&self, _menu: Menu, _pos: Point) {
        warn!("show_context_menu unimplemented for web");
    }

    pub fn set_title(&self, title: &str) {
        if let Some(state) = self.0.upgrade() {
            state.canvas.set_title(title)
        }
    }
}

unsafe impl Send for IdleHandle {}
unsafe impl Sync for IdleHandle {}

impl IdleHandle {
    /// Add an idle handler, which is called (once) when the main thread is idle.
    pub fn add_idle_callback<F>(&self, callback: F)
    where
        F: FnOnce(&mut dyn WinHandler) + Send + 'static,
    {
        let mut queue = self.queue.lock().expect("IdleHandle::add_idle queue");
        queue.push(IdleKind::Callback(Box::new(callback)));

        if queue.len() == 1 {
            if let Some(window_state) = self.state.upgrade() {
                let state = window_state.clone();
                window_state
                    .request_animation_frame(move || {
                        state.process_idle_queue();
                    })
                    .expect("request_animation_frame failed");
            }
        }
    }

    pub fn add_idle_token(&self, token: IdleToken) {
        let mut queue = self.queue.lock().expect("IdleHandle::add_idle queue");
        queue.push(IdleKind::Token(token));

        if queue.len() == 1 {
            if let Some(window_state) = self.state.upgrade() {
                let state = window_state.clone();
                window_state
                    .request_animation_frame(move || {
                        state.process_idle_queue();
                    })
                    .expect("request_animation_frame failed");
            }
        }
    }
}

fn get_button(button: i16) -> Option<PointerButton> {
    match button {
        0 => Some(PointerButton::Primary),
        1 => Some(PointerButton::Auxiliary),
        2 => Some(PointerButton::Secondary),
        3 => Some(PointerButton::X1),
        4 => Some(PointerButton::X2),
        _ => None,
    }
}

fn get_buttons(mask: u16) -> PointerButtons {
    let mut buttons = PointerButtons::new();
    if mask & 1 != 0 {
        buttons.insert(PointerButton::Primary);
    }
    if mask & 1 << 1 != 0 {
        buttons.insert(PointerButton::Secondary);
    }
    if mask & 1 << 2 != 0 {
        buttons.insert(PointerButton::Auxiliary);
    }
    if mask & 1 << 3 != 0 {
        buttons.insert(PointerButton::X1);
    }
    if mask & 1 << 4 != 0 {
        buttons.insert(PointerButton::X2);
    }
    buttons
}

fn set_cursor(canvas: &web_sys::HtmlCanvasElement, cursor: &Cursor) {
    canvas
        .style()
        .set_property(
            "cursor",
            #[allow(deprecated)]
            match cursor {
                Cursor::Arrow => "default",
                Cursor::IBeam => "text",
                Cursor::Pointer => "pointer",
                Cursor::Crosshair => "crosshair",
                Cursor::OpenHand => "grab",
                Cursor::NotAllowed => "not-allowed",
                Cursor::ResizeLeftRight => "ew-resize",
                Cursor::ResizeUpDown => "ns-resize",
                // TODO: support custom cursors
                Cursor::Custom(_) => "default",
            },
        )
        .unwrap_or_else(|_| warn!("Failed to set cursor"));
}
