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

//! X11 window creation and window management.

use std::collections::BinaryHeap;
use std::convert::TryFrom;
use std::num::NonZero;
use std::os::unix::io::RawFd;
use std::panic::Location;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use crate::backend::shared::xkb::{xkb_simulate_input, KeyEventsState};
use crate::pointer::{
    Angle, MouseInfo, PenInclination, PenInfo, PointerId, PointerType, TouchInfo,
};
use crate::scale::Scalable;
use anyhow::{anyhow, Context, Error};
use flo_binding::{Binding, Bound, MutableBound};
use tracing::{error, warn};
use x11rb::connection::Connection;
use x11rb::errors::ReplyOrIdError;
use x11rb::properties::{WmHints, WmHintsState, WmSizeHints};
use x11rb::protocol::render::Pictformat;
use x11rb::protocol::xinput::{self, DeviceType, ModifierInfo, TouchEventFlags};
use x11rb::protocol::xproto::{
    self, AtomEnum, ChangeWindowAttributesAux, ColormapAlloc, ConfigureNotifyEvent,
    ConfigureWindowAux, ConnectionExt, EventMask, ImageOrder as X11ImageOrder, KeyButMask,
    PropMode, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::xcb_ffi::XCBConnection;

use raw_window_handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    XcbDisplayHandle, XcbWindowHandle,
};

use crate::backend::shared::Timer;
use crate::common_util::IdleCallback;
use crate::dialog::FileDialogOptions;
use crate::error::Error as ShellError;
use crate::keyboard::{KeyState, Modifiers};
use crate::kurbo::{Insets, Point, Rect, Size, Vec2};
use crate::mouse::{Cursor, CursorDesc};
use crate::region::Region;
use crate::scale::Scale;
use crate::text::Event;
use crate::window::{
    FileDialogToken, IdleToken, TextFieldToken, TimerToken, WinHandler, WindowLevel,
};
use crate::{window, PointerButton, PointerButtons, PointerEvent, ScaledArea};

use super::application::Application;
use super::dialog;
use super::menu::Menu;

fn size_hints(resizable: bool, size: Size, min_size: Size) -> WmSizeHints {
    let mut size_hints = WmSizeHints::new();
    if resizable {
        size_hints.min_size = Some((min_size.width as i32, min_size.height as i32));
    } else {
        size_hints.min_size = Some((size.width as i32, size.height as i32));
        size_hints.max_size = Some((size.width as i32, size.height as i32));
    }
    size_hints
}

pub(crate) struct WindowBuilder {
    app: Application,
    handler: Option<Box<dyn WinHandler>>,
    title: String,
    transparent: bool,
    position: Option<Point>,
    size: Size,
    min_size: Size,
    resizable: bool,
    level: WindowLevel,
    state: Option<window::WindowState>,
}

impl WindowBuilder {
    pub fn new(app: Application) -> WindowBuilder {
        WindowBuilder {
            app,
            handler: None,
            title: String::new(),
            transparent: false,
            position: None,
            size: Size::new(500.0, 400.0),
            min_size: Size::new(0.0, 0.0),
            resizable: true,
            level: WindowLevel::AppWindow,
            state: None,
        }
    }

    pub fn handler(mut self, handler: Box<dyn WinHandler>) -> Self {
        self.handler = Some(handler);
        self
    }

    pub fn size(mut self, size: Size) -> Self {
        // zero sized window results in server error
        self.size = if size.width == 0. || size.height == 0. {
            Size::new(1., 1.)
        } else {
            size
        };
        self
    }

    pub fn min_size(mut self, min_size: Size) -> Self {
        self.min_size = min_size;
        self
    }

    pub fn resizable(mut self, resizable: bool) -> Self {
        self.resizable = resizable;
        self
    }

    pub fn show_titlebar(self, _show_titlebar: bool) -> Self {
        // not sure how to do this, maybe _MOTIF_WM_HINTS?
        warn!("WindowBuilder::show_titlebar is currently unimplemented for X11 backend.");
        self
    }

    pub fn transparent(mut self, transparent: bool) -> Self {
        self.transparent = transparent;
        self
    }

    pub fn position(mut self, position: Point) -> Self {
        self.position = Some(position);
        self
    }

    pub fn level(mut self, level: WindowLevel) -> Self {
        self.level = level;
        self
    }

    pub fn window_state(mut self, state: window::WindowState) -> Self {
        self.state = Some(state);
        self
    }

    pub fn title<S: Into<String>>(mut self, title: S) -> Self {
        self.title = title.into();
        self
    }

    pub fn menu(self, _menu: Menu) -> Self {
        // TODO(x11/menus): implement WindowBuilder::set_menu (currently a no-op)
        warn!("WindowBuilder::menu is currently unimplemented for X11 backend.");
        self
    }

    // TODO(x11/menus): make menus if requested
    pub fn build(self) -> Result<WindowHandle, Error> {
        let conn = self.app.connection();
        let screen_num = self.app.screen_num();
        let id = conn.generate_id()?;
        let setup = conn.setup();

        let scale_override = std::env::var("GLAZIER_OVERRIDE_SCALE")
            .ok()
            .map(|x| x.parse::<f64>());

        let scale =
            match scale_override.or_else(|| self.app.rdb.get_value("Xft.dpi", "").transpose()) {
                Some(Ok(dpi)) => {
                    let scale = dpi / 96.;
                    Scale::new(scale, scale)
                }
                None => Scale::default(),
                Some(Err(err)) => {
                    let default = Scale::default();
                    warn!(
                        "Unable to parse dpi: {:?}, defaulting to {:?}",
                        err, default
                    );
                    default
                }
            };

        let size_px = self.size.to_px(scale);
        let screen = setup
            .roots
            .get(screen_num)
            .ok_or_else(|| anyhow!("Invalid screen num: {}", screen_num))?;
        let visual_type = if self.transparent {
            self.app.argb_visual_type()
        } else {
            None
        };
        let (transparent, visual_type) = match visual_type {
            Some(visual) => (true, visual),
            None => (false, self.app.root_visual_type()),
        };
        if transparent != self.transparent {
            warn!("Windows with transparent backgrounds do not work");
        }

        let mut cw_values = xproto::CreateWindowAux::new().event_mask(
            EventMask::EXPOSURE
                | EventMask::STRUCTURE_NOTIFY
                | EventMask::KEY_PRESS
                | EventMask::KEY_RELEASE
                | EventMask::FOCUS_CHANGE
                | EventMask::LEAVE_WINDOW,
        );
        if transparent {
            let colormap = conn.generate_id()?;
            conn.create_colormap(
                ColormapAlloc::NONE,
                colormap,
                screen.root,
                visual_type.visual_id,
            )?;
            cw_values = cw_values
                .border_pixel(screen.white_pixel)
                .colormap(colormap);
        };

        let (parent, parent_origin) = match &self.level {
            WindowLevel::AppWindow => (None, Vec2::ZERO),
            WindowLevel::Tooltip(parent)
            | WindowLevel::DropDown(parent)
            | WindowLevel::Modal(parent) => {
                let handle = parent.0.unwrap_x11().window.clone();
                let origin = handle
                    .clone()
                    .map(|x| x.get_position())
                    .unwrap_or_default()
                    .to_vec2();
                (handle, origin)
            }
        };
        let pos = (self.position.unwrap_or_default() + parent_origin).to_px(scale);

        // Create the actual window
        let (width_px, height_px) = (size_px.width as u16, size_px.height as u16);
        let depth = if transparent { 32 } else { screen.root_depth };
        conn.create_window(
            // Window depth
            depth,
            // The new window's ID
            id,
            // Parent window of this new window
            // TODO(#468): either `screen.root()` (no parent window) or pass parent here to attach
            screen.root,
            // X-coordinate of the new window
            pos.x as _,
            // Y-coordinate of the new window
            pos.y as _,
            // Width of the new window
            width_px,
            // Height of the new window
            height_px,
            // Border width
            0,
            // Window class type
            WindowClass::INPUT_OUTPUT,
            // Visual ID
            visual_type.visual_id,
            // Window properties mask
            &cw_values,
        )?
        .check()
        .context("create window")?;

        super::pointer::enable_window_pointers(conn, id)?;

        if let Some(colormap) = cw_values.colormap {
            conn.free_colormap(colormap)?;
        }

        let handler = RwLock::new(self.handler.unwrap());
        // Initialize some properties
        let atoms = self.app.atoms();
        let pid = nix::unistd::Pid::this().as_raw();
        if let Ok(pid) = u32::try_from(pid) {
            conn.change_property32(
                PropMode::REPLACE,
                id,
                atoms._NET_WM_PID,
                AtomEnum::CARDINAL,
                &[pid],
            )?
            .check()
            .context("set _NET_WM_PID")?;
        }

        if let Some(name) = std::env::args_os().next() {
            // ICCCM § 4.1.2.5:
            // The WM_CLASS property (of type STRING without control characters) contains two
            // consecutive null-terminated strings. These specify the Instance and Class names.
            //
            // The code below just imitates what happens on the gtk backend:
            // - instance: The program's name
            // - class: The program's name with first letter in upper case

            // Get the name of the running binary
            let path: &std::path::Path = name.as_ref();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");

            // Build the contents of WM_CLASS
            let mut wm_class = Vec::with_capacity(2 * (name.len() + 1));
            wm_class.extend(name.as_bytes());
            wm_class.push(0);
            if let Some(&first) = wm_class.first() {
                wm_class.push(first.to_ascii_uppercase());
                wm_class.extend(&name.as_bytes()[1..]);
            }
            wm_class.push(0);
            conn.change_property8(
                PropMode::REPLACE,
                id,
                AtomEnum::WM_CLASS,
                AtomEnum::STRING,
                &wm_class,
            )?;
        } else {
            // GTK (actually glib) goes fishing in /proc (platform_get_argv0()). We pass.
        }

        // Replace the window's WM_PROTOCOLS with the following.
        let protocols = [atoms.WM_DELETE_WINDOW];
        conn.change_property32(
            PropMode::REPLACE,
            id,
            atoms.WM_PROTOCOLS,
            AtomEnum::ATOM,
            &protocols,
        )?
        .check()
        .context("set WM_PROTOCOLS")?;

        let min_size = self.min_size.to_px(scale);
        log_x11!(size_hints(self.resizable, size_px, min_size)
            .set_normal_hints(conn, id)
            .context("set wm normal hints"));

        // TODO: set _NET_WM_STATE
        let mut hints = WmHints::new();
        if let Some(state) = self.state {
            hints.initial_state = Some(match state {
                window::WindowState::Maximized => WmHintsState::Normal,
                window::WindowState::Minimized => WmHintsState::Iconic,
                window::WindowState::Restored => WmHintsState::Normal,
            });
        }
        log_x11!(hints.set(conn, id).context("set wm hints"));

        // set level
        {
            let window_type = match self.level {
                WindowLevel::AppWindow => atoms._NET_WM_WINDOW_TYPE_NORMAL,
                WindowLevel::Tooltip(_) => atoms._NET_WM_WINDOW_TYPE_TOOLTIP,
                WindowLevel::Modal(_) => atoms._NET_WM_WINDOW_TYPE_DIALOG,
                WindowLevel::DropDown(_) => atoms._NET_WM_WINDOW_TYPE_DROPDOWN_MENU,
            };

            let conn = self.app.connection();
            log_x11!(conn.change_property32(
                xproto::PropMode::REPLACE,
                id,
                atoms._NET_WM_WINDOW_TYPE,
                AtomEnum::ATOM,
                &[window_type],
            ));
            if matches!(
                self.level,
                WindowLevel::DropDown(_) | WindowLevel::Modal(_) | WindowLevel::Tooltip(_)
            ) {
                log_x11!(conn.change_window_attributes(
                    id,
                    &ChangeWindowAttributesAux::new().override_redirect(1),
                ));
            }
        }

        let window = Arc::new(Window {
            id,
            app: self.app.clone(),
            handler,
            area: Binding::new(ScaledArea::from_px(size_px, scale)),
            scale: Binding::new(scale),
            min_size,
            invalid: RwLock::new(Region::EMPTY),
            destroyed: Binding::new(false),
            timer_queue: Mutex::new(BinaryHeap::new()),
            idle_queue: Arc::new(Mutex::new(Vec::new())),
            idle_pipe: self.app.idle_pipe(),
            next_text_field: Binding::new(None),
            active_text_field: Binding::new(None),
            need_to_reset_compose: Binding::new(false),
            parent,
        });

        window.set_title(&self.title);
        if let Some(pos) = self.position {
            window.set_position(pos);
        }

        let handle = WindowHandle::new(id, visual_type.visual_id, window.clone());
        window.connect(handle.clone())?;

        self.app.add_window(id, window)?;

        Ok(handle)
    }
}

/// An X11 window.
//
// We use lots of RefCells here, so to avoid panics we need some rules. The basic observation is
// that there are two ways we can end up calling the code in this file:
//
// 1) it either comes from the system (e.g. through some X11 event), or
// 2) from the client (e.g. druid, calling a method on its `WindowHandle`).
//
// Note that 2 only ever happens as a result of 1 (i.e., the system calls us, we call the client
// using the `WinHandler`, and it calls us back). The rules are:
//
// a) We never call into the system as a result of 2. As a consequence, we never get 1
//    re-entrantly.
// b) We *almost* never call into the `WinHandler` while holding any of the other RefCells. There's
//    an exception for `paint`. This is enforced by the `with_handler` method.
//    (TODO: we could try to encode this exception statically, by making the data accessible in
//    case 2 smaller than the data accessible in case 1).
pub(crate) struct Window {
    id: u32,
    app: Application,
    handler: RwLock<Box<dyn WinHandler>>,
    area: Binding<ScaledArea>,
    scale: Binding<Scale>,
    // min size in px
    min_size: Size,
    /// We've told X11 to destroy this window, so don't so any more X requests with this window id.
    destroyed: Binding<bool>,
    /// The region that was invalidated since the last time we rendered.
    invalid: RwLock<Region>,
    /// Timers, sorted by "earliest deadline first"
    timer_queue: Mutex<BinaryHeap<Timer<()>>>,
    idle_queue: Arc<Mutex<Vec<IdleKind>>>,
    // Writing to this wakes up the event loop, so that it can run idle handlers.
    idle_pipe: RawFd,
    next_text_field: Binding<Option<TextFieldToken>>,
    active_text_field: Binding<Option<TextFieldToken>>,
    need_to_reset_compose: Binding<bool>,
    parent: Option<Arc<Window>>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct CustomCursor(xproto::Cursor);

impl Window {
    #[track_caller]
    fn with_handler<T, F: FnOnce(&mut dyn WinHandler) -> T>(&self, f: F) -> Option<T> {
        // FIXME: what is that
        let lock = self.invalid.write();
        if lock.is_err() {
            error!("other RefCells were borrowed when calling into the handler");
            return None;
        }
        std::mem::drop(lock);

        self.with_handler_and_dont_check_the_other_borrows(f)
    }

    #[track_caller]
    fn with_handler_and_dont_check_the_other_borrows<T, F: FnOnce(&mut dyn WinHandler) -> T>(
        &self,
        f: F,
    ) -> Option<T> {
        match self.handler.write() {
            Ok(mut h) => Some(f(&mut **h)),
            Err(_) => {
                error!("failed to borrow WinHandler at {}", Location::caller());
                None
            }
        }
    }

    fn connect(&self, handle: WindowHandle) -> Result<(), Error> {
        let size = self.size().size_dp();
        let scale = self.scale.get();
        self.with_handler(|h| {
            h.connect(&handle.into());
            h.scale(scale);
            h.size(size);
        });
        Ok(())
    }

    /// Start the destruction of the window.
    pub fn destroy(&self) {
        if !self.destroyed() {
            self.destroyed.set(true);
            log_x11!(self.app.connection().destroy_window(self.id));
        }
    }

    fn destroyed(&self) -> bool {
        self.destroyed.get()
    }

    fn size(&self) -> ScaledArea {
        self.area.get()
    }

    // note: size is in px
    fn size_changed(&self, size: Size) -> Result<(), Error> {
        let scale = self.scale.get();
        let new_size = {
            if size != self.area.get().size_px() {
                self.area.set(ScaledArea::from_px(size, scale));
                true
            } else {
                false
            }
        };
        if new_size {
            self.add_invalid_rect(size.to_dp(scale).to_rect())?;
            self.with_handler(|h| h.size(size.to_dp(scale)));
            self.with_handler(|h| h.scale(scale));
        }
        Ok(())
    }

    fn render(&self) -> Result<(), Error> {
        self.with_handler(|h| h.prepare_paint());

        if self.destroyed() {
            return Ok(());
        }

        let invalid = std::mem::replace(&mut *borrow_mut!(self.invalid)?, Region::EMPTY);
        self.with_handler_and_dont_check_the_other_borrows(|handler| {
            handler.paint(&invalid);
        });

        Ok(())
    }

    fn show(&self) {
        if !self.destroyed() {
            log_x11!(self.app.connection().map_window(self.id));
        }
    }

    fn close(&self) {
        self.destroy();
    }

    /// Set whether the window should be resizable
    fn resizable(&self, resizable: bool) {
        let conn = self.app.connection();
        log_x11!(size_hints(resizable, self.size().size_px(), self.min_size)
            .set_normal_hints(conn, self.id)
            .context("set normal hints"));
    }

    /// Set whether the window should show titlebar
    fn show_titlebar(&self, _show_titlebar: bool) {
        warn!("Window::show_titlebar is currently unimplemented for X11 backend.");
    }

    fn parent_origin(&self) -> Vec2 {
        self.parent
            .clone()
            .map(|x| x.get_position())
            .unwrap_or_default()
            .to_vec2()
    }

    fn get_position(&self) -> Point {
        fn _get_position(window: &Window) -> Result<Point, Error> {
            let conn = window.app.connection();
            let scale = window.scale.get();
            let geom = conn.get_geometry(window.id)?.reply()?;
            let cord = conn
                .translate_coordinates(window.id, geom.root, 0, 0)?
                .reply()?;
            Ok(Point::new(cord.dst_x as _, cord.dst_y as _).to_dp(scale))
        }
        let pos = _get_position(self);
        log_x11!(&pos);
        pos.map(|pos| pos - self.parent_origin())
            .unwrap_or_default()
    }

    fn set_position(&self, pos: Point) {
        let conn = self.app.connection();
        let scale = self.scale.get();
        let pos = (pos + self.parent_origin()).to_px(scale).expand();
        log_x11!(conn.configure_window(
            self.id,
            &ConfigureWindowAux::new().x(pos.x as i32).y(pos.y as i32),
        ));
    }

    fn set_size(&self, size: Size) {
        let conn = self.app.connection();
        let scale = self.scale.get();
        let size = size.to_px(scale).expand();
        log_x11!(conn.configure_window(
            self.id,
            &ConfigureWindowAux::new()
                .width(size.width as u32)
                .height(size.height as u32),
        ));
    }

    /// Bring this window to the front of the window stack and give it focus.
    fn bring_to_front_and_focus(&self) {
        if self.destroyed() {
            return;
        }

        // TODO(x11/misc): Unsure if this does exactly what the doc comment says; need a test case.
        let conn = self.app.connection();
        log_x11!(conn.configure_window(
            self.id,
            &xproto::ConfigureWindowAux::new().stack_mode(xproto::StackMode::ABOVE),
        ));
        log_x11!(conn.set_input_focus(
            xproto::InputFocus::POINTER_ROOT,
            self.id,
            xproto::Time::CURRENT_TIME,
        ));
    }

    fn add_invalid_rect(&self, rect: Rect) -> Result<(), Error> {
        let scale = self.scale.get();
        self.invalid
            .write()
            .map_err(|_| {
                anyhow::Error::msg(format!(
                    "[{}:{}] {}",
                    std::file!(),
                    std::line!(),
                    std::stringify!($val)
                ))
            })?
            .add_rect(rect.to_px(scale).expand().to_dp(scale));
        Ok(())
    }

    /// Redraw more-or-less now.
    ///
    /// "More-or-less" because if we're already waiting on a present, we defer the drawing until it
    /// completes.
    fn redraw_now(&self) -> Result<(), Error> {
        self.render()?;
        Ok(())
    }

    /// Schedule a redraw on the idle loop, or if we are waiting on present then schedule it for
    /// when the current present finishes.
    fn request_anim_frame(&self) {
        let idle = IdleHandle {
            queue: Arc::clone(&self.idle_queue),
            pipe: self.idle_pipe,
        };
        idle.schedule_redraw();
    }

    fn invalidate(&self) {
        let rect = self.size().size_dp().to_rect();
        self.add_invalid_rect(rect)
            .unwrap_or_else(|err| error!("Window::invalidate - failed to invalidate: {}", err));

        self.request_anim_frame();
    }

    fn invalidate_rect(&self, rect: Rect) {
        if let Err(err) = self.add_invalid_rect(rect) {
            error!("Window::invalidate_rect - failed to enlarge rect: {}", err);
        }

        self.request_anim_frame();
    }

    fn set_title(&self, title: &str) {
        if self.destroyed() {
            return;
        }

        let atoms = self.app.atoms();

        // This is technically incorrect. STRING encoding is *not* UTF8. However, I am not sure
        // what it really is. WM_LOCALE_NAME might be involved. Hopefully, nothing cares about this
        // as long as _NET_WM_NAME is also set (which uses UTF8).
        log_x11!(self.app.connection().change_property8(
            xproto::PropMode::REPLACE,
            self.id,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            title.as_bytes(),
        ));
        log_x11!(self.app.connection().change_property8(
            xproto::PropMode::REPLACE,
            self.id,
            atoms._NET_WM_NAME,
            atoms.UTF8_STRING,
            title.as_bytes(),
        ));
    }

    fn set_cursor(&self, cursor: &Cursor) {
        let cursors = &self.app.cursors;
        #[allow(deprecated)]
        let cursor = match cursor {
            Cursor::Arrow => cursors.default,
            Cursor::IBeam => cursors.text,
            Cursor::Pointer => cursors.pointer,
            Cursor::Crosshair => cursors.crosshair,
            Cursor::OpenHand => {
                warn!("Cursor::OpenHand not supported for x11 backend. using arrow cursor");
                None
            }
            Cursor::NotAllowed => cursors.not_allowed,
            Cursor::ResizeLeftRight => cursors.col_resize,
            Cursor::ResizeUpDown => cursors.row_resize,
            Cursor::Custom(custom) => Some(custom.unwrap_x11().0),
        };
        if cursor.is_none() {
            warn!("Unable to load cursor {:?}", cursor);
            return;
        }
        let conn = self.app.connection();
        let changes = ChangeWindowAttributesAux::new().cursor(cursor);
        if let Err(e) = conn.change_window_attributes(self.id, &changes) {
            error!("Changing cursor window attribute failed {}", e);
        };
    }

    fn set_menu(&self, _menu: Menu) {
        // TODO(x11/menus): implement Window::set_menu (currently a no-op)
    }

    fn get_scale(&self) -> Result<Scale, Error> {
        Ok(self.scale.get())
    }

    pub fn handle_expose(&self, expose: &xproto::ExposeEvent) -> Result<(), Error> {
        let rect = Rect::from_origin_size(
            (expose.x as f64, expose.y as f64),
            (expose.width as f64, expose.height as f64),
        )
        .to_dp(self.scale.get());

        self.add_invalid_rect(rect)?;
        if expose.count == 0 {
            self.request_anim_frame();
        }
        Ok(())
    }

    pub fn handle_key_event(
        &self,
        scancode: u32,
        xkb_state: &mut KeyEventsState,
        key_state: KeyState,
        is_repeat: bool,
    ) {
        // This is a horrible hack, but the X11 backend is not actively maintained anyway
        self.with_handler(|handler| {
            let keysym = xkb_state.get_one_sym(scancode);
            let event = xkb_state.key_event(scancode, keysym, key_state, is_repeat);
            match key_state {
                KeyState::Down => {
                    if handler.key_down(&event) {
                        // The keypress was handled by the user, nothing to do
                        return;
                    }
                    let next_field = self.reset_text_fields_if_needed(xkb_state, handler);

                    let Some(field_token) = next_field else {
                        // We're not in a text field, therefore, we don't want to compose
                        // This does mean that we don't get composition outside of a text field
                        // but that's expected, as there is no suitable `handler` method for that
                        // case. We get the same behaviour on macOS (?)
                        return;
                    };
                    let mut input_handler = handler.acquire_input_lock(field_token, true);
                    // Because there is no *other* IME on this backend, we meet the criteria for this method
                    xkb_simulate_input(xkb_state, keysym, &event, &mut *input_handler);
                    handler.release_input_lock(field_token);
                }
                KeyState::Up => {
                    handler.key_up(&event);
                    self.reset_text_fields_if_needed(xkb_state, handler);
                }
            }
        });
    }

    fn reset_text_fields_if_needed(
        &self,
        xkb_state: &mut KeyEventsState,
        handler: &mut dyn WinHandler,
    ) -> Option<TextFieldToken> {
        let next_field = self.next_text_field.get();
        let need_to_reset_compose = self.need_to_reset_compose.get();
        {
            let previous_field = self.active_text_field.get();
            // In theory, this should be more proactive - but I'm not sure how to implement that
            // and researching that isn't a high priority
            if next_field != previous_field {
                // If the active field has changed, the composition doesn't make any sense
                if xkb_state.cancel_composing() {
                    // If we previously were composing, the previous field must have existed
                    // However, the previous field may also have been deleted, so we need to only
                    // reset it if it were enabled
                    if let Some(previous) = previous_field {
                        let mut ime = handler.acquire_input_lock(previous, true);
                        ime.set_composition_range(None);
                        handler.release_input_lock(previous);
                    }
                }
                self.active_text_field.set(next_field);
            }
        }
        // Shadow previous, as we know it may be outdated, and text_field should be used instead
        if need_to_reset_compose && xkb_state.cancel_composing() {
            if let Some(text_field) = next_field {
                // Please note: This might be superfluous
                let mut ime = handler.acquire_input_lock(text_field, true);
                ime.set_composition_range(None);
                handler.release_input_lock(text_field);
            }
        }
        next_field
    }

    fn base_pointer_event(
        &self,
        x: i32,
        y: i32,
        mods: ModifierInfo,
        detail: u32,
        src_id: u16,
    ) -> PointerEvent {
        // In x11rb, xinput x and y coordinates are i32's but in the protocol they're fixed-precision FP1616s
        // https://github.com/psychon/x11rb/blob/dacfba5e2a8eef4b80df75d9bec9061c3d98d279/xcb-proto-1.15.2/src/xinput.xml#L2374
        let (ev_x, ev_y) = (x as f64 / 65536.0, y as f64 / 65536.0);
        let scale = self.scale.get();
        let mods = mods.base | mods.locked | mods.latched;
        // TODO: what are the high 16 bits for? Maybe virtual modifiers?
        let mods = (mods as u16).into();
        let button = pointer_button(detail);

        PointerEvent {
            pointer_id: PointerId(src_id as u64),
            is_primary: false,
            pointer_type: PointerType::Mouse(MouseInfo {
                wheel_delta: Default::default(),
            }),
            pos: Point::new(ev_x, ev_y).to_dp(scale),
            buttons: pointer_buttons(mods),
            modifiers: key_mods(mods),
            button,
            focus: false,
            count: 0,
        }
    }

    fn pointer_touch_event(&self, ev: &xinput::TouchBeginEvent) -> PointerEvent {
        // TODO: I think future x11rb will have BitAnd?
        let is_primary = (ev.flags | TouchEventFlags::TOUCH_EMULATING_POINTER) == ev.flags;
        let pointer_type = PointerType::Touch(TouchInfo {
            contact_geometry: Size::ZERO,
            pressure: 0.0,
        });
        let button = if is_primary {
            PointerButton::Primary
        } else {
            PointerButton::None
        };

        PointerEvent {
            is_primary,
            pointer_type,
            button,
            pointer_id: PointerId(ev.sourceid as u64 | (ev.detail as u64) << 32),
            ..self.base_pointer_event(ev.event_x, ev.event_y, ev.mods, ev.detail, ev.sourceid)
        }
    }

    fn pointer_event(&self, ev: &xinput::ButtonPressEvent) -> PointerEvent {
        let device = self.app.pointer_device(ev.deviceid);
        let src_device = self.app.pointer_device(ev.sourceid);

        let is_primary = if let Some(device) = device {
            device.device_type == DeviceType::MASTER_POINTER
        } else {
            tracing::warn!(
                "got event for device {}, but no such device exists",
                ev.deviceid
            );
            false
        };

        let pointer_type = if let Some(src_device) = src_device {
            let pressure = src_device.valuators.pressure.as_ref().map_or(0.0, |val| {
                let raw = val.read(&ev.axisvalues).unwrap_or(val.min);
                // Scale to the range [0.0, 1.0].
                (raw - val.min) / (val.max - val.min)
            });
            let x_tilt = src_device
                .valuators
                .x_tilt
                .as_ref()
                .map_or(0.0, |val| val.read(&ev.axisvalues).unwrap_or(0.0));
            let y_tilt = src_device
                .valuators
                .y_tilt
                .as_ref()
                .map_or(0.0, |val| val.read(&ev.axisvalues).unwrap_or(0.0));

            let inclination = PenInclination::from_tilt(x_tilt, y_tilt).unwrap_or_default();

            let pen_info = PenInfo {
                pressure,
                tangential_pressure: 0.0,
                inclination,
                twist: Angle::degrees(0.0),
            };

            match src_device.device_kind {
                super::pointer::DeviceKind::Pen => PointerType::Pen(pen_info),
                super::pointer::DeviceKind::Eraser => PointerType::Eraser(pen_info),
                // TODO: support touch
                super::pointer::DeviceKind::Touch | super::pointer::DeviceKind::Mouse => {
                    PointerType::Mouse(MouseInfo {
                        wheel_delta: Vec2::ZERO,
                    })
                }
            }
        } else {
            PointerType::Mouse(MouseInfo {
                wheel_delta: Vec2::ZERO,
            })
        };

        PointerEvent {
            is_primary,
            pointer_type,
            ..self.base_pointer_event(ev.event_x, ev.event_y, ev.mods, ev.detail, ev.sourceid)
        }
    }

    pub fn handle_button_press(&self, ev: &xinput::ButtonPressEvent) -> Result<(), Error> {
        let mut pointer_ev = self.pointer_event(ev);
        // The xcb state field doesn't include the newly pressed button, but
        // druid wants it to be included.
        pointer_ev.buttons = pointer_ev.buttons.with(pointer_ev.button);
        // TODO: detect the count
        pointer_ev.count = 1;
        self.with_handler(|h| h.pointer_down(&pointer_ev));
        Ok(())
    }

    pub fn handle_button_release(&self, ev: &xinput::ButtonPressEvent) -> Result<(), Error> {
        let mut pointer_ev = self.pointer_event(ev);
        // The xcb state includes the newly released button, but druid
        // doesn't want it.
        pointer_ev.buttons = pointer_ev.buttons.without(pointer_ev.button);
        self.with_handler(|h| h.pointer_up(&pointer_ev));
        Ok(())
    }

    pub fn handle_touch_begin(&self, ev: &xinput::TouchBeginEvent) -> Result<(), Error> {
        let mut pointer_ev = self.pointer_touch_event(ev);
        pointer_ev.buttons = pointer_ev.buttons.with(pointer_ev.button);
        self.with_handler(|h| h.pointer_down(&pointer_ev));
        Ok(())
    }

    pub fn handle_touch_update(&self, ev: &xinput::TouchBeginEvent) -> Result<(), Error> {
        let pointer_ev = self.pointer_touch_event(ev);
        self.with_handler(|h| h.pointer_move(&pointer_ev));
        Ok(())
    }

    pub fn handle_touch_end(&self, ev: &xinput::TouchBeginEvent) -> Result<(), Error> {
        let mut pointer_ev = self.pointer_touch_event(ev);
        pointer_ev.buttons = pointer_ev.buttons.without(pointer_ev.button);
        self.with_handler(|h| h.pointer_move(&pointer_ev));
        Ok(())
    }

    pub fn handle_wheel(&self, ev: &xinput::ButtonPressEvent) -> Result<(), Error> {
        let mut pointer_ev = self.pointer_event(ev);

        // We use a delta of 120 per tick to match the behavior of Windows.
        let is_shift = pointer_ev.modifiers.shift();
        let delta = match ev.detail {
            4 if is_shift => (-120.0, 0.0),
            4 => (0.0, -120.0),
            5 if is_shift => (120.0, 0.0),
            5 => (0.0, 120.0),
            6 => (-120.0, 0.0),
            7 => (120.0, 0.0),
            _ => return Err(anyhow!("unexpected mouse wheel button: {}", ev.detail)),
        };
        pointer_ev.pointer_type = PointerType::Mouse(MouseInfo {
            wheel_delta: delta.into(),
        });
        pointer_ev.button = PointerButton::None;

        self.with_handler(|h| h.wheel(&pointer_ev));
        Ok(())
    }

    pub fn handle_motion_notify(&self, ev: &xinput::ButtonPressEvent) -> Result<(), Error> {
        let mut pointer_ev = self.pointer_event(ev);
        pointer_ev.button = PointerButton::None;
        self.with_handler(|h| h.pointer_move(&pointer_ev));
        Ok(())
    }

    pub fn handle_leave_notify(
        &self,
        _leave_notify: &xproto::LeaveNotifyEvent,
    ) -> Result<(), Error> {
        self.with_handler(|h| h.pointer_leave());
        Ok(())
    }

    pub fn handle_got_focus(&self) {
        self.with_handler(|h| h.got_focus());
    }

    pub fn handle_lost_focus(&self, xkb_state: &mut KeyEventsState) {
        self.with_handler(|h| {
            h.lost_focus();
            let active = self.active_text_field.get();
            if let Some(field) = active {
                if xkb_state.cancel_composing() {
                    let mut ime = h.acquire_input_lock(field, true);
                    let range = ime.composition_range();
                    // If we were composing, a composition range must have been set.
                    // To be safe, avoid unwrapping it anyway
                    if let Some(range) = range {
                        // replace_range resets the composition string
                        ime.replace_range(range, xkb_state.cancelled_string());
                    } else {
                        ime.set_composition_range(None);
                    }

                    h.release_input_lock(field);
                }
            }
        });
    }

    pub fn handle_client_message(&self, client_message: &xproto::ClientMessageEvent) {
        // https://www.x.org/releases/X11R7.7/doc/libX11/libX11/libX11.html#id2745388
        // https://www.x.org/releases/X11R7.6/doc/xorg-docs/specs/ICCCM/icccm.html#window_deletion
        let atoms = self.app.atoms();
        if client_message.type_ == atoms.WM_PROTOCOLS && client_message.format == 32 {
            let protocol = client_message.data.as_data32()[0];
            if protocol == atoms.WM_DELETE_WINDOW {
                self.with_handler(|h| h.request_close());
            }
        }
    }

    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn handle_destroy_notify(&self, _destroy_notify: &xproto::DestroyNotifyEvent) {
        self.with_handler(|h| h.destroy());
    }

    pub fn handle_configure_notify(&self, event: &ConfigureNotifyEvent) -> Result<(), Error> {
        self.size_changed(Size::new(event.width as f64, event.height as f64))
    }

    pub(crate) fn run_idle(&self) {
        let mut queue = Vec::new();
        std::mem::swap(&mut *self.idle_queue.lock().unwrap(), &mut queue);

        let mut needs_redraw = false;
        self.with_handler(|handler| {
            for callback in queue {
                match callback {
                    IdleKind::Callback(f) => {
                        f(handler);
                    }
                    IdleKind::Token(tok) => {
                        handler.idle(tok);
                    }
                    IdleKind::Redraw => {
                        needs_redraw = true;
                    }
                }
            }
        });

        if needs_redraw {
            if let Err(e) = self.redraw_now() {
                error!("Error redrawing: {}", e);
            }
        }
    }

    pub(crate) fn next_timeout(&self) -> Option<Instant> {
        self.timer_queue
            .lock()
            .unwrap()
            .peek()
            .map(|timer| timer.deadline())
    }

    pub(crate) fn run_timers(&self, now: Instant) {
        while let Some(deadline) = self.next_timeout() {
            if deadline > now {
                break;
            }
            // Remove the timer and get the token
            let token = self.timer_queue.lock().unwrap().pop().unwrap().token();
            self.with_handler(|h| h.timer(token));
        }
    }
}

// Converts from, e.g., the `details` field of `xcb::xproto::ButtonPressEvent`
fn pointer_button(button: u32) -> PointerButton {
    match button {
        0 => PointerButton::None,
        1 => PointerButton::Primary,
        2 => PointerButton::Auxiliary,
        3 => PointerButton::Secondary,
        // buttons 4 through 7 are for scrolling.
        4..=7 => PointerButton::None,
        8 => PointerButton::X1,
        9 => PointerButton::X2,
        _ => {
            warn!("unknown pointer button code {}", button);
            PointerButton::None
        }
    }
}

// Extracts the pointer buttons from, e.g., the `state` field of
// `xcb::xproto::ButtonPressEvent`
fn pointer_buttons(mods: KeyButMask) -> PointerButtons {
    let mut buttons = PointerButtons::new();
    let button_masks = &[
        (xproto::ButtonMask::M1, PointerButton::Primary),
        (xproto::ButtonMask::M2, PointerButton::Auxiliary),
        (xproto::ButtonMask::M3, PointerButton::Secondary),
        // TODO: determine the X1/X2 state, using our own caching if necessary.
        // BUTTON_MASK_4/5 do not work: they are for scroll events.
    ];
    for (mask, button) in button_masks {
        // TODO: future x11rb will have more convenient bitmasks
        if u16::from(mods) & u16::from(*mask) != 0 {
            buttons.insert(*button);
        }
    }
    buttons
}

// Extracts the keyboard modifiers from, e.g., the `state` field of
// `xcb::xproto::ButtonPressEvent`
fn key_mods(mods: KeyButMask) -> Modifiers {
    let mut ret = Modifiers::default();
    let mut key_masks = [
        (xproto::ModMask::SHIFT, Modifiers::SHIFT),
        (xproto::ModMask::CONTROL, Modifiers::CONTROL),
        // X11's mod keys are configurable, but this seems
        // like a reasonable default for US keyboards, at least,
        // where the "windows" key seems to be MOD_MASK_4.
        (xproto::ModMask::M1, Modifiers::ALT),
        (xproto::ModMask::M2, Modifiers::NUM_LOCK),
        (xproto::ModMask::M4, Modifiers::META),
        (xproto::ModMask::LOCK, Modifiers::CAPS_LOCK),
    ];
    for (mask, modifiers) in &mut key_masks {
        // TODO: future x11rb will have more convenient bitmasks
        if u16::from(mods) & u16::from(*mask) != 0 {
            ret |= *modifiers;
        }
    }
    ret
}

/// A handle that can get used to schedule an idle handler. Note that
/// this handle can be cloned and sent between threads.
#[derive(Clone)]
pub struct IdleHandle {
    queue: Arc<Mutex<Vec<IdleKind>>>,
    pipe: RawFd,
}

pub(crate) enum IdleKind {
    Callback(IdleCallback),
    Token(IdleToken),
    Redraw,
}

impl IdleHandle {
    fn wake(&self) {
        loop {
            match nix::unistd::write(self.pipe, &[0]) {
                Err(nix::errno::Errno::EINTR) => {}
                Err(nix::errno::Errno::EAGAIN) => {}
                Err(e) => {
                    error!("Failed to write to idle pipe: {}", e);
                    break;
                }
                Ok(_) => {
                    break;
                }
            }
        }
    }

    pub(crate) fn schedule_redraw(&self) {
        self.add_idle(IdleKind::Redraw);
    }

    pub fn add_idle_callback<F>(&self, callback: F)
    where
        F: FnOnce(&mut dyn WinHandler) + Send + 'static,
    {
        self.add_idle(IdleKind::Callback(Box::new(callback)));
    }

    pub fn add_idle_token(&self, token: IdleToken) {
        self.add_idle(IdleKind::Token(token));
    }

    fn add_idle(&self, idle: IdleKind) {
        self.queue.lock().unwrap().push(idle);
        self.wake();
    }
}

#[derive(Clone, Default)]
pub(crate) struct WindowHandle {
    id: u32,
    #[allow(dead_code)] // Only used with the raw-win-handle feature
    visual_id: u32,
    window: Option<Arc<Window>>,
}
impl PartialEq for WindowHandle {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for WindowHandle {}

impl WindowHandle {
    fn new(id: u32, visual_id: u32, window: Arc<Window>) -> WindowHandle {
        WindowHandle {
            id,
            visual_id,
            window: Some(window),
        }
    }

    pub fn show(&self) {
        if let Some(w) = &self.window {
            w.show();
        } else {
            error!("Window {} has already been dropped", self.id);
        }
    }

    pub fn close(&self) {
        if let Some(w) = &self.window {
            w.close();
        } else {
            error!("Window {} has already been dropped", self.id);
        }
    }

    pub fn resizable(&self, resizable: bool) {
        if let Some(w) = &self.window {
            w.resizable(resizable);
        } else {
            error!("Window {} has already been dropped", self.id);
        }
    }

    pub fn show_titlebar(&self, show_titlebar: bool) {
        if let Some(w) = &self.window {
            w.show_titlebar(show_titlebar);
        } else {
            error!("Window {} has already been dropped", self.id);
        }
    }

    pub fn set_position(&self, position: Point) {
        if let Some(w) = &self.window {
            w.set_position(position);
        } else {
            error!("Window {} has already been dropped", self.id);
        }
    }

    pub fn get_position(&self) -> Point {
        if let Some(w) = &self.window {
            w.get_position()
        } else {
            error!("Window {} has already been dropped", self.id);
            Point::new(0.0, 0.0)
        }
    }

    pub fn content_insets(&self) -> Insets {
        warn!("WindowHandle::content_insets unimplemented for X11 backend.");
        Insets::ZERO
    }

    pub fn set_size(&self, size: Size) {
        if let Some(w) = &self.window {
            w.set_size(size);
        } else {
            error!("Window {} has already been dropped", self.id);
        }
    }

    pub fn get_size(&self) -> Size {
        if let Some(w) = &self.window {
            w.size().size_dp()
        } else {
            error!("Window {} has already been dropped", self.id);
            Size::ZERO
        }
    }

    pub fn set_window_state(&self, _state: window::WindowState) {
        warn!("WindowHandle::set_window_state is currently unimplemented for X11 backend.");
    }

    pub fn get_window_state(&self) -> window::WindowState {
        warn!("WindowHandle::get_window_state is currently unimplemented for X11 backend.");
        window::WindowState::Restored
    }

    pub fn handle_titlebar(&self, _val: bool) {
        warn!("WindowHandle::handle_titlebar is currently unimplemented for X11 backend.");
    }

    pub fn bring_to_front_and_focus(&self) {
        if let Some(w) = &self.window {
            w.bring_to_front_and_focus();
        } else {
            error!("Window {} has already been dropped", self.id);
        }
    }

    pub fn request_anim_frame(&self) {
        if let Some(w) = &self.window {
            w.request_anim_frame();
        } else {
            error!("Window {} has already been dropped", self.id);
        }
    }

    pub fn invalidate(&self) {
        if let Some(w) = &self.window {
            w.invalidate();
        } else {
            error!("Window {} has already been dropped", self.id);
        }
    }

    pub fn invalidate_rect(&self, rect: Rect) {
        if let Some(w) = &self.window {
            w.invalidate_rect(rect);
        } else {
            error!("Window {} has already been dropped", self.id);
        }
    }

    pub fn set_title(&self, title: &str) {
        if let Some(w) = &self.window {
            w.set_title(title);
        } else {
            error!("Window {} has already been dropped", self.id);
        }
    }

    pub fn set_menu(&self, menu: Menu) {
        if let Some(w) = &self.window {
            w.set_menu(menu);
        } else {
            error!("Window {} has already been dropped", self.id);
        }
    }

    pub fn add_text_field(&self) -> TextFieldToken {
        TextFieldToken::next()
    }

    pub fn remove_text_field(&self, token: TextFieldToken) {
        if let Some(window) = &self.window {
            if window.next_text_field.get() == Some(token) {
                window.next_text_field.set(None);
                window.need_to_reset_compose.set(true);
            }
            if window.active_text_field.get() == Some(token) {
                window.active_text_field.set(None);
                window.need_to_reset_compose.set(true);
            }
        }
    }

    pub fn set_focused_text_field(&self, active_field: Option<TextFieldToken>) {
        if let Some(window) = &self.window {
            window.next_text_field.set(active_field);
        }
    }

    pub fn update_text_field(&self, token: TextFieldToken, _update: Event) {
        if let Some(window) = &self.window {
            // This should be active rather than passive, but since the X11 backend is
            // low-maintenance, this is fine
            if window.active_text_field.get() == Some(token) {
                window.need_to_reset_compose.set(true);
            }
            // If a different text field were updated, we don't care about that case
            // as we only have the one composition state
        }
    }

    pub fn request_timer(&self, deadline: Instant) -> TimerToken {
        if let Some(w) = &self.window {
            let timer = Timer::new(deadline, ());
            w.timer_queue.lock().unwrap().push(timer);
            timer.token()
        } else {
            TimerToken::INVALID
        }
    }

    pub fn set_cursor(&mut self, cursor: &Cursor) {
        if let Some(w) = &self.window {
            w.set_cursor(cursor);
        }
    }

    pub fn make_cursor(&self, desc: &CursorDesc) -> Option<Cursor> {
        if let Some(w) = &self.window {
            match w.app.render_argb32_pictformat_cursor() {
                None => {
                    warn!("Custom cursors are not supported by the X11 server");
                    None
                }
                Some(format) => {
                    let conn = w.app.connection();
                    let setup = &conn.setup();
                    let screen = &setup.roots[w.app.screen_num()];
                    match make_cursor(conn, setup.image_byte_order, screen.root, format, desc) {
                        // TODO: We 'leak' the cursor - nothing ever calls render_free_cursor
                        Ok(cursor) => Some(cursor),
                        Err(err) => {
                            error!("Failed to create custom cursor: {:?}", err);
                            None
                        }
                    }
                }
            }
        } else {
            None
        }
    }

    pub fn open_file(&mut self, options: FileDialogOptions) -> Option<FileDialogToken> {
        if let Some(w) = &self.window {
            if let Some(idle) = self.get_idle_handle() {
                Some(dialog::open_file(w.id, idle, options))
            } else {
                warn!("Couldn't open file because no idle handle available");
                None
            }
        } else {
            None
        }
    }

    pub fn save_as(&mut self, options: FileDialogOptions) -> Option<FileDialogToken> {
        if let Some(w) = &self.window {
            if let Some(idle) = self.get_idle_handle() {
                Some(dialog::save_file(w.id, idle, options))
            } else {
                warn!("Couldn't save file because no idle handle available");
                None
            }
        } else {
            None
        }
    }

    pub fn show_context_menu(&self, _menu: Menu, _pos: Point) {
        // TODO(x11/menus): implement WindowHandle::show_context_menu
        warn!("WindowHandle::show_context_menu is currently unimplemented for X11 backend.");
    }

    pub fn get_idle_handle(&self) -> Option<IdleHandle> {
        self.window.as_ref().map(|w| IdleHandle {
            queue: Arc::clone(&w.idle_queue),
            pipe: w.idle_pipe,
        })
    }

    pub fn get_scale(&self) -> Result<Scale, ShellError> {
        if let Some(w) = &self.window {
            Ok(w.get_scale()?)
        } else {
            error!("Window {} has already been dropped", self.id);
            Ok(Scale::new(1.0, 1.0))
        }
    }
}

impl HasWindowHandle for WindowHandle {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let mut handle = XcbWindowHandle::new(NonZero::new(self.id).unwrap());
        handle.visual_id = NonZero::new(self.visual_id);

        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(RawWindowHandle::Xcb(handle)) })
    }
}

impl HasDisplayHandle for WindowHandle {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        if let Some(window) = self.window.clone() {
            let screen = window.app.screen_num();
            let connection = window.app.connection().get_raw_xcb_connection();
            let handle = XcbDisplayHandle::new(NonNull::new(connection), screen as i32);
            Ok(unsafe { DisplayHandle::borrow_raw(RawDisplayHandle::Xcb(handle)) })
        } else {
            // Documentation for HasRawWindowHandle encourages filling in all fields possible,
            // leaving those empty that cannot be derived.
            error!("Failed to get XCBConnection, returning incomplete handle");
            Err(raw_window_handle::HandleError::Unavailable)
        }
    }
}
fn make_cursor(
    _conn: &XCBConnection,
    _byte_order: X11ImageOrder,
    _root_window: u32,
    _argb32_format: Pictformat,
    _desc: &CursorDesc,
) -> Result<Cursor, ReplyOrIdError> {
    Ok(Cursor::Arrow)
}
