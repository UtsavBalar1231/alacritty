//! Terminal window context.

use std::error::Error;
use std::fs::File;
use std::io::Write;
use std::mem;
#[cfg(not(windows))]
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use glutin::config::Config as GlutinConfig;
use glutin::display::GetGlDisplay;
#[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
use glutin::platform::x11::X11GlConfigExt;
use log::info;
use serde_json as json;
use winit::event::{ElementState, Event as WinitEvent, Modifiers, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::raw_window_handle::HasDisplayHandle;
use winit::window::WindowId;

use alacritty_terminal::event::{Event as TerminalEvent, OnResize};
use alacritty_terminal::event_loop::{EventLoop as PtyEventLoop, Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Direction;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::tty;

use crate::cli::{ParsedOptions, WindowOptions};
use crate::clipboard::Clipboard;
use crate::config::UiConfig;
use crate::display::Display;
use crate::display::window::Window;
use crate::event::{
    ActionContext, Event, EventProxy, EventType, InlineSearchState, Mouse, SearchState,
    TouchPurpose,
};
#[cfg(unix)]
use crate::logging::LOG_TARGET_IPC_CONFIG;
use crate::message_bar::MessageBuffer;
use crate::scheduler::Scheduler;
use crate::tabs::{TabId, TabManager, TabSelection, TerminalId};
use crate::{input, renderer};

/// Terminal session state associated with a window.
struct TerminalSession {
    terminal: Arc<FairMutex<Term<EventProxy>>>,
    cursor_blink_timed_out: bool,
    prev_bell_cmd: Option<Instant>,
    inline_search_state: InlineSearchState,
    search_state: SearchState,
    notifier: Notifier,
    preserve_title: bool,
    #[cfg(not(windows))]
    master_fd: RawFd,
    #[cfg(not(windows))]
    shell_pid: u32,
    window_config: ParsedOptions,
    config: Rc<UiConfig>,
}

impl TerminalSession {
    /// Create a new terminal session.
    fn new(
        display: &Display,
        config: Rc<UiConfig>,
        options: &WindowOptions,
        proxy: EventLoopProxy<Event>,
        tab_id: TabId,
        terminal_id: TerminalId,
    ) -> Result<Self, Box<dyn Error>> {
        let mut pty_config = config.pty_config();
        options.terminal_options.override_pty_config(&mut pty_config);

        let preserve_title = options.window_identity.title.is_some();

        info!(
            "PTY dimensions: {:?} x {:?}",
            display.size_info.screen_lines(),
            display.size_info.columns()
        );

        let event_proxy = EventProxy::new(proxy, display.window.id(), tab_id, terminal_id);

        // Create the terminal.
        //
        // This object contains all of the state about what's being displayed. It's
        // wrapped in a clonable mutex since both the I/O loop and display need to
        // access it.
        let terminal = Term::new(config.term_options(), &display.size_info, event_proxy.clone());
        let terminal = Arc::new(FairMutex::new(terminal));

        // Create the PTY.
        //
        // The PTY forks a process to run the shell on the slave side of the
        // pseudoterminal. A file descriptor for the master side is retained for
        // reading/writing to the shell.
        let pty = tty::new(&pty_config, display.size_info.into(), display.window.id().into())?;

        #[cfg(not(windows))]
        let master_fd = pty.file().as_raw_fd();
        #[cfg(not(windows))]
        let shell_pid = pty.child().id();

        // Create the pseudoterminal I/O loop.
        //
        // PTY I/O is ran on another thread as to not occupy cycles used by the
        // renderer and input processing. Note that access to the terminal state is
        // synchronized since the I/O loop updates the state, and the display
        // consumes it periodically.
        let event_loop = PtyEventLoop::new(
            Arc::clone(&terminal),
            event_proxy.clone(),
            pty,
            pty_config.drain_on_exit,
            config.debug.ref_test,
        )?;

        // The event loop channel allows write requests from the event processor
        // to be sent to the pty loop and ultimately written to the pty.
        let loop_tx = event_loop.channel();

        // Kick off the I/O thread.
        let _io_thread = event_loop.spawn();

        // Start cursor blinking, in case `Focused` isn't sent on startup.
        if config.cursor.style().blinking {
            event_proxy.send_event(TerminalEvent::CursorBlinkingChange.into());
        }

        Ok(Self {
            preserve_title,
            terminal,
            #[cfg(not(windows))]
            master_fd,
            #[cfg(not(windows))]
            shell_pid,
            config,
            notifier: Notifier(loop_tx),
            cursor_blink_timed_out: Default::default(),
            prev_bell_cmd: Default::default(),
            inline_search_state: Default::default(),
            window_config: Default::default(),
            search_state: Default::default(),
        })
    }

    fn update_config(&mut self, new_config: Rc<UiConfig>) -> Rc<UiConfig> {
        let old_config = mem::replace(&mut self.config, new_config);

        // Apply ipc config if there are overrides.
        self.config = self.window_config.override_config_rc(self.config.clone());
        self.terminal.lock().set_options(self.config.term_options());

        old_config
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        // Shutdown the terminal's PTY.
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}

/// Event context for one individual Alacritty window.
pub struct WindowContext {
    pub message_buffer: MessageBuffer,
    pub display: Display,
    pub dirty: bool,
    event_queue: Vec<WinitEvent<Event>>,
    tabs: TabManager<TerminalSession>,
    tab_bar: crate::display::TabBarState,
    modifiers: Modifiers,
    mouse: Mouse,
    touch: TouchPurpose,
    occluded: bool,
}

impl WindowContext {
    /// Create initial window context that does bootstrapping the graphics API we're going to use.
    pub fn initial(
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<Event>,
        config: Rc<UiConfig>,
        mut options: WindowOptions,
        tab_id: TabId,
        terminal_id: TerminalId,
    ) -> Result<Self, Box<dyn Error>> {
        let raw_display_handle = event_loop.display_handle().unwrap().as_raw();

        let mut identity = config.window.identity.clone();
        options.window_identity.override_identity_config(&mut identity);

        // Windows has different order of GL platform initialization compared to any other platform;
        // it requires the window first.
        #[cfg(windows)]
        let window = Window::new(event_loop, &config, &identity, &mut options)?;
        #[cfg(windows)]
        let raw_window_handle = Some(window.raw_window_handle());

        #[cfg(not(windows))]
        let raw_window_handle = None;

        let gl_display = renderer::platform::create_gl_display(
            raw_display_handle,
            raw_window_handle,
            config.debug.prefer_egl,
        )?;
        let gl_config = renderer::platform::pick_gl_config(&gl_display, raw_window_handle)?;

        #[cfg(not(windows))]
        let window = Window::new(
            event_loop,
            &config,
            &identity,
            &mut options,
            #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
            gl_config.x11_visual(),
        )?;

        // Create context.
        let gl_context =
            renderer::platform::create_gl_context(&gl_display, &gl_config, raw_window_handle)?;

        let display = Display::new(window, gl_context, &config, false)?;

        Self::new(display, config, options, proxy, tab_id, terminal_id)
    }

    /// Create additional context with the graphics platform other windows are using.
    pub fn additional(
        gl_config: &GlutinConfig,
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<Event>,
        config: Rc<UiConfig>,
        mut options: WindowOptions,
        config_overrides: ParsedOptions,
        tab_id: TabId,
        terminal_id: TerminalId,
    ) -> Result<Self, Box<dyn Error>> {
        let gl_display = gl_config.display();

        let mut identity = config.window.identity.clone();
        options.window_identity.override_identity_config(&mut identity);

        // Check if new window will be opened as a tab.
        // This must be done before `Window::new()`, which unsets `window_tabbing_id`.
        #[cfg(target_os = "macos")]
        let tabbed = options.window_tabbing_id.is_some();
        #[cfg(not(target_os = "macos"))]
        let tabbed = false;

        let window = Window::new(
            event_loop,
            &config,
            &identity,
            &mut options,
            #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
            gl_config.x11_visual(),
        )?;

        // Create context.
        let raw_window_handle = window.raw_window_handle();
        let gl_context =
            renderer::platform::create_gl_context(&gl_display, gl_config, Some(raw_window_handle))?;

        let display = Display::new(window, gl_context, &config, tabbed)?;

        let mut window_context = Self::new(display, config, options, proxy, tab_id, terminal_id)?;

        // Set the config overrides at startup.
        //
        // These are already applied to `config`, so no update is necessary.
        window_context.active_session_mut().window_config = config_overrides;

        Ok(window_context)
    }

    fn active_session(&self) -> &TerminalSession {
        self.tabs.active().expect("window has at least one tab").value()
    }

    fn active_session_mut(&mut self) -> &mut TerminalSession {
        self.tabs.active_mut().expect("window has at least one tab").value_mut()
    }

    pub fn active_tab_id(&self) -> Option<TabId> {
        self.tabs.active_id()
    }

    /// Create a new terminal window context.
    fn new(
        display: Display,
        config: Rc<UiConfig>,
        options: WindowOptions,
        proxy: EventLoopProxy<Event>,
        tab_id: TabId,
        terminal_id: TerminalId,
    ) -> Result<Self, Box<dyn Error>> {
        let session = TerminalSession::new(&display, config, &options, proxy, tab_id, terminal_id)?;
        let mut tabs = TabManager::new();
        tabs.open_with_id(tab_id, session);

        // Create context for the Alacritty window.
        Ok(WindowContext {
            display,
            message_buffer: Default::default(),
            event_queue: Default::default(),
            modifiers: Default::default(),
            occluded: Default::default(),
            mouse: Default::default(),
            touch: Default::default(),
            dirty: Default::default(),
            tabs,
            tab_bar: Default::default(),
        })
    }

    /// Update the terminal window to the latest config.
    pub fn update_config(&mut self, new_config: Rc<UiConfig>) {
        let active_id = self.tabs.active_id();
        let mut active_old_config = None;
        for tab in self.tabs.iter_mut() {
            let old_config = tab.value_mut().update_config(new_config.clone());
            if Some(tab.id()) == active_id {
                active_old_config = Some(old_config);
            }
        }

        let old_config = active_old_config.expect("window has an active tab");
        let config = self.active_session().config.clone();

        self.display.update_config(&config);

        // Reload cursor if its thickness has changed.
        if (old_config.cursor.thickness() - config.cursor.thickness()).abs() > f32::EPSILON {
            self.display.pending_update.set_cursor_dirty();
        }

        if old_config.font != config.font {
            let scale_factor = self.display.window.scale_factor as f32;
            // Do not update font size if it has been changed at runtime.
            if self.display.font_size == old_config.font.size().scale(scale_factor) {
                self.display.font_size = config.font.size().scale(scale_factor);
            }

            let font = config.font.clone().with_size(self.display.font_size);
            self.display.pending_update.set_font(font);
        }

        // Always reload the theme to account for auto-theme switching.
        self.display.window.set_theme(config.window.theme());

        // Update display if either padding options or resize increments were changed.
        let window_config = &old_config.window;
        if window_config.padding(1.) != config.window.padding(1.)
            || window_config.dynamic_padding != config.window.dynamic_padding
            || window_config.resize_increments != config.window.resize_increments
        {
            self.display.pending_update.dirty = true;
        }

        // Update title on config reload according to the following table.
        //
        // │cli │ dynamic_title │ current_title == old_config ││ set_title │
        // │ Y  │       _       │              _              ││     N     │
        // │ N  │       Y       │              Y              ││     Y     │
        // │ N  │       Y       │              N              ││     N     │
        // │ N  │       N       │              _              ││     Y     │
        if !self.active_session().preserve_title
            && (!config.window.dynamic_title
                || self.display.window.title() == old_config.window.identity.title)
        {
            self.display.window.set_title(config.window.identity.title.clone());
        }

        let opaque = config.window_opacity() >= 1.;

        // Disable shadows for transparent windows on macOS.
        #[cfg(target_os = "macos")]
        self.display.window.set_has_shadow(opaque);

        #[cfg(target_os = "macos")]
        self.display.window.set_option_as_alt(config.window.option_as_alt());

        // Change opacity and blur state.
        self.display.window.set_transparent(!opaque);
        self.display.window.set_blur(config.window.blur);

        // Update hint keys.
        self.display.hint_state.update_alphabet(config.hints.alphabet());

        // Update cursor blinking.
        let event = Event::new(TerminalEvent::CursorBlinkingChange.into(), None);
        self.event_queue.push(event.into());

        self.dirty = true;
    }

    /// Get reference to the window's configuration.
    #[cfg(unix)]
    pub fn config(&self) -> &UiConfig {
        &self.active_session().config
    }

    /// Clear the window config overrides.
    #[cfg(unix)]
    pub fn reset_window_config(&mut self, config: Rc<UiConfig>) {
        // Clear previous window errors.
        self.message_buffer.remove_target(LOG_TARGET_IPC_CONFIG);

        for tab in self.tabs.iter_mut() {
            tab.value_mut().window_config.clear();
        }

        // Reload current config to pull new IPC config.
        self.update_config(config);
    }

    /// Add new window config overrides.
    #[cfg(unix)]
    pub fn add_window_config(&mut self, config: Rc<UiConfig>, options: &ParsedOptions) {
        // Clear previous window errors.
        self.message_buffer.remove_target(LOG_TARGET_IPC_CONFIG);

        for tab in self.tabs.iter_mut() {
            tab.value_mut().window_config.extend_from_slice(options);
        }

        // Reload current config to pull new IPC config.
        self.update_config(config);
    }

    pub fn create_tab(
        &mut self,
        options: WindowOptions,
        proxy: EventLoopProxy<Event>,
        tab_id: TabId,
        terminal_id: TerminalId,
    ) -> Result<(), Box<dyn Error>> {
        let focused = self.active_session().terminal.lock().is_focused;
        let mut config_overrides = options.config_overrides();
        let mut config = self.active_session().config.clone();
        config = config_overrides.override_config_rc(config);

        let mut session =
            TerminalSession::new(&self.display, config, &options, proxy, tab_id, terminal_id)?;
        session.window_config = config_overrides;
        session.terminal.lock().is_focused = focused;

        if let Some(active_id) = self.tabs.active_id() {
            self.tabs.get_mut(active_id).unwrap().value_mut().terminal.lock().is_focused = false;
        }

        self.tabs.open_with_id(tab_id, session);
        self.mark_tab_change_dirty();

        Ok(())
    }

    pub fn select_tab(&mut self, selection: TabSelection) {
        let old_active_id = match self.tabs.active_id() {
            Some(active_id) => active_id,
            None => return,
        };
        let focused = self.active_session().terminal.lock().is_focused;

        if self.tabs.select(selection).is_err() || self.tabs.active_id() == Some(old_active_id) {
            return;
        }

        self.tabs.get_mut(old_active_id).unwrap().value_mut().terminal.lock().is_focused = false;
        self.active_session_mut().terminal.lock().is_focused = focused;
        self.mark_tab_change_dirty();
        self.resize_all_sessions();
    }

    pub fn close_tab(&mut self, tab_id: TabId) -> bool {
        if self.tabs.len() <= 1 {
            return false;
        }

        let was_active = self.tabs.active_id() == Some(tab_id);
        let focused = self.active_session().terminal.lock().is_focused;
        if self.tabs.close(tab_id).is_err() {
            return false;
        }

        if was_active {
            self.active_session_mut().terminal.lock().is_focused = focused;
        }
        self.mark_tab_change_dirty();

        true
    }

    pub fn close_active_tab(&mut self) -> bool {
        match self.active_tab_id() {
            Some(tab_id) => self.close_tab(tab_id),
            None => false,
        }
    }

    pub fn close_window(&mut self) {
        self.display.window.hold = false;
        self.tabs
            .active_mut()
            .expect("window has an active tab")
            .value_mut()
            .terminal
            .lock()
            .exit();
    }

    pub fn request_redraw(&mut self) {
        self.dirty = true;
        if self.display.window.has_frame {
            self.display.window.request_redraw();
        }
    }

    fn resize_session(session: &mut TerminalSession, size: crate::display::SizeInfo) {
        session.terminal.lock().resize(size);
        session.notifier.on_resize(size.into());
    }

    fn resize_all_sessions(&mut self) {
        let size = self.display.size_info;
        for tab in self.tabs.iter_mut() {
            Self::resize_session(tab.value_mut(), size);
        }
    }

    pub fn is_active_tab(&self, tab_id: Option<TabId>) -> bool {
        tab_id.is_none_or(|tab_id| self.tabs.active_id() == Some(tab_id))
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn has_tab(&self, tab_id: TabId) -> bool {
        self.tabs.get(tab_id).is_some()
    }

    fn tab_bar_hit_test(&self) -> Option<TabId> {
        let size = self.display.size_info;
        let search_lines = usize::from(self.active_session().search_state.regex().is_some());
        let message_lines = self.message_buffer.message().map_or(0, |m| m.text(&size).len());
        let tab_bar_line = size.screen_lines() + search_lines + message_lines;
        let tab_bar_y = (size.padding_y() + tab_bar_line as f32 * size.cell_height()) as usize;
        if !(self.mouse.y >= tab_bar_y && self.mouse.y < tab_bar_y + size.cell_height() as usize) {
            return None;
        }

        if self.mouse.x < size.padding_x() as usize
            || self.mouse.x
                >= (size.padding_x() + size.columns() as f32 * size.cell_width()) as usize
        {
            return None;
        }

        let x = (self.mouse.x as f32 - size.padding_x()) / size.cell_width();
        self.tab_bar
            .hit_regions
            .iter()
            .find(|(_, start, end)| (x as usize) >= *start && (x as usize) < *end)
            .map(|(tab_id, ..)| *tab_id)
    }

    fn mark_tab_change_dirty(&mut self) {
        self.display.cursor_hidden = false;
        self.display.damage_tracker.frame().mark_fully_damaged();
        self.display.pending_update.dirty = true;
        self.dirty = true;
    }

    /// Draw the window.
    pub fn draw(&mut self, scheduler: &mut Scheduler) {
        self.display.window.requested_redraw = false;

        if self.occluded {
            return;
        }

        self.dirty = false;

        // Force the display to process any pending display update.
        self.display.process_renderer_update();

        // Request immediate re-draw if visual bell animation is not finished yet.
        if !self.display.visual_bell.completed() {
            // We can get an OS redraw which bypasses alacritty's frame throttling, thus
            // marking the window as dirty when we don't have frame yet.
            if self.display.window.has_frame {
                self.display.window.request_redraw();
            } else {
                self.dirty = true;
            }
        }

        // Redraw the window.
        let active_tab_id = self.tabs.active_id();
        self.tab_bar.tabs = self
            .tabs
            .iter()
            .map(|tab| crate::display::TabBarEntry {
                id: tab.id(),
                label: tab.value().config.window.identity.title.clone(),
                active: active_tab_id == Some(tab.id()),
            })
            .collect();
        self.tab_bar.hit_regions.clear();

        let session = self.tabs.active_mut().expect("window has an active tab").value_mut();
        let terminal = session.terminal.lock();
        self.display.draw(
            terminal,
            scheduler,
            &self.message_buffer,
            &session.config,
            &mut session.search_state,
            Some(&mut self.tab_bar),
        );
        self.resize_all_sessions();
    }

    /// Process events for this terminal window.
    pub fn handle_event(
        &mut self,
        #[cfg(target_os = "macos")] event_loop: &ActiveEventLoop,
        event_proxy: &EventLoopProxy<Event>,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
        event: WinitEvent<Event>,
    ) {
        match event {
            WinitEvent::WindowEvent {
                event: WindowEvent::CursorMoved { position, .. }, ..
            } => {
                self.mouse.x = position.x.max(0.0) as usize;
                self.mouse.y = position.y.max(0.0) as usize;
                self.event_queue.push(event);
                return;
            },
            WinitEvent::WindowEvent {
                event:
                    WindowEvent::MouseInput {
                        state: ElementState::Pressed,
                        button: MouseButton::Left,
                        ..
                    },
                ..
            } if self.tab_bar_hit_test().is_some() => {
                if let Some(tab_id) = self.tab_bar_hit_test() {
                    self.select_tab(TabSelection::Id(tab_id));
                }
                return;
            },
            WinitEvent::AboutToWait
            | WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. } => {
                // Continue to process any pending display updates, even without queued events.
            },
            event => {
                self.event_queue.push(event);
                return;
            },
        }

        let current_active_tab = self.tabs.active_id();
        let tab_ids = self.tabs.iter().map(|tab| tab.id()).collect::<Vec<_>>();
        let mut events = Vec::with_capacity(self.event_queue.len());
        for event in self.event_queue.drain(..) {
            if let WinitEvent::UserEvent(event) = &event
                && matches!(event.payload(), EventType::Terminal(_))
                && let Some(event_tab_id) = event.tab_id()
                && (current_active_tab != Some(event_tab_id) || !tab_ids.contains(&event_tab_id))
            {
                continue;
            }

            events.push(event);
        }

        let tab_bar_visible = self.tabs.len() >= 2;
        let session = self.tabs.active_mut().expect("window has an active tab").value_mut();
        let mut terminal = session.terminal.lock();

        let old_is_searching = session.search_state.history_index.is_some();

        let context = ActionContext {
            cursor_blink_timed_out: &mut session.cursor_blink_timed_out,
            prev_bell_cmd: &mut session.prev_bell_cmd,
            message_buffer: &mut self.message_buffer,
            inline_search_state: &mut session.inline_search_state,
            search_state: &mut session.search_state,
            modifiers: &mut self.modifiers,
            notifier: &mut session.notifier,
            display: &mut self.display,
            mouse: &mut self.mouse,
            touch: &mut self.touch,
            dirty: &mut self.dirty,
            occluded: &mut self.occluded,
            terminal: &mut terminal,
            #[cfg(not(windows))]
            master_fd: session.master_fd,
            #[cfg(not(windows))]
            shell_pid: session.shell_pid,
            preserve_title: session.preserve_title,
            config: &session.config,
            event_proxy,
            #[cfg(target_os = "macos")]
            event_loop,
            clipboard,
            scheduler,
        };
        let mut processor = input::Processor::new(context);

        for event in events {
            processor.handle_event(event);
        }

        // Process DisplayUpdate events.
        if self.display.pending_update.dirty {
            Self::submit_display_update(
                &mut terminal,
                &mut self.display,
                &mut session.notifier,
                &self.message_buffer,
                &mut session.search_state,
                old_is_searching,
                &session.config,
                tab_bar_visible,
            );
            self.dirty = true;
        }

        if self.dirty || self.mouse.hint_highlight_dirty {
            self.dirty |= self.display.update_highlighted_hints(
                &terminal,
                &session.config,
                &self.mouse,
                self.modifiers.state(),
            );
            self.mouse.hint_highlight_dirty = false;
        }

        // Don't call `request_redraw` when event is `RedrawRequested` since the `dirty` flag
        // represents the current frame, but redraw is for the next frame.
        if self.dirty
            && self.display.window.has_frame
            && !self.occluded
            && !matches!(event, WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. })
        {
            self.display.window.request_redraw();
        }
    }

    /// ID of this terminal context.
    pub fn id(&self) -> WindowId {
        self.display.window.id()
    }

    /// Write the ref test results to the disk.
    pub fn write_ref_test_results(&self) {
        // Dump grid state.
        let mut grid = self.active_session().terminal.lock().grid().clone();
        grid.initialize_all();
        grid.truncate();

        let serialized_grid = json::to_string(&grid).expect("serialize grid");

        let size_info = &self.display.size_info;
        let size = TermSize::new(size_info.columns(), size_info.screen_lines());
        let serialized_size = json::to_string(&size).expect("serialize size");

        let serialized_config = format!("{{\"history_size\":{}}}", grid.history_size());

        File::create("./grid.json")
            .and_then(|mut f| f.write_all(serialized_grid.as_bytes()))
            .expect("write grid.json");

        File::create("./size.json")
            .and_then(|mut f| f.write_all(serialized_size.as_bytes()))
            .expect("write size.json");

        File::create("./config.json")
            .and_then(|mut f| f.write_all(serialized_config.as_bytes()))
            .expect("write config.json");
    }

    /// Submit the pending changes to the `Display`.
    fn submit_display_update(
        terminal: &mut Term<EventProxy>,
        display: &mut Display,
        notifier: &mut Notifier,
        message_buffer: &MessageBuffer,
        search_state: &mut SearchState,
        old_is_searching: bool,
        config: &UiConfig,
        tab_bar_visible: bool,
    ) {
        // Compute cursor positions before resize.
        let num_lines = terminal.screen_lines();
        let cursor_at_bottom = terminal.grid().cursor.point.line + 1 == num_lines;
        let origin_at_bottom = if terminal.mode().contains(TermMode::VI) {
            terminal.vi_mode_cursor.point.line == num_lines - 1
        } else {
            search_state.direction == Direction::Left
        };

        display.handle_update(
            terminal,
            notifier,
            message_buffer,
            search_state,
            config,
            tab_bar_visible,
        );

        let new_is_searching = search_state.history_index.is_some();
        if !old_is_searching && new_is_searching {
            // Scroll on search start to make sure origin is visible with minimal viewport motion.
            let display_offset = terminal.grid().display_offset();
            if display_offset == 0 && cursor_at_bottom && !origin_at_bottom {
                terminal.scroll_display(Scroll::Delta(1));
            } else if display_offset != 0 && origin_at_bottom {
                terminal.scroll_display(Scroll::Delta(-1));
            }
        }
    }
}
