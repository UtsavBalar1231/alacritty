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
#[cfg(not(any(target_os = "macos", windows)))]
use winit::event_loop::AsyncRequestSerial;
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::raw_window_handle::HasDisplayHandle;
use winit::window::WindowId;

use alacritty_terminal::event::{Event as TerminalEvent, Notify, OnResize};
use alacritty_terminal::event_loop::{EventLoop as PtyEventLoop, Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Direction;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::NamedColor;

use crate::cli::{ParsedOptions, WindowOptions};
use crate::clipboard::Clipboard;
use crate::config::UiConfig;
use crate::config::window::{TabBarConfig, TabBarVisibility};
use crate::display::Display;
use crate::display::window::Window;
use crate::event::{
    ActionContext, Event, EventProxy, EventType, InlineSearchState, Mouse, SearchState,
    TouchPurpose,
};
#[cfg(not(any(target_os = "macos", windows)))]
use crate::event::{ActivationOpenRequest, PendingActivationOpen};
#[cfg(unix)]
use crate::logging::LOG_TARGET_IPC_CONFIG;
use crate::message_bar::MessageBuffer;
use crate::scheduler::Scheduler;
use crate::tabs::{SessionIds, Tab, TabError, TabId, TabManager, TabSelection, TerminalId};
use crate::{input, renderer};

#[cfg(unix)]
use crate::polling::ipc::TabInfo;

fn visible_title(
    static_title: &str,
    dynamic_title: bool,
    preserve_title: bool,
    terminal_title: Option<&str>,
) -> String {
    if dynamic_title && !preserve_title {
        terminal_title.unwrap_or(static_title).to_owned()
    } else {
        static_title.to_owned()
    }
}

fn format_tab_title(
    template: &str,
    index: usize,
    title: &str,
    activity: &str,
    bell: &str,
) -> String {
    let mut rendered = String::new();
    let mut remaining = template;
    while !remaining.is_empty() {
        if let Some(placeholder) = remaining.strip_prefix("{index}") {
            push_tab_title_value(&mut rendered, &(index + 1).to_string());
            remaining = placeholder;
        } else if let Some(placeholder) = remaining.strip_prefix("{zero_index}") {
            push_tab_title_value(&mut rendered, &index.to_string());
            remaining = placeholder;
        } else if let Some(placeholder) = remaining.strip_prefix("{title}") {
            push_tab_title_value(&mut rendered, title);
            remaining = placeholder;
        } else if let Some(placeholder) = remaining.strip_prefix("{activity}") {
            push_tab_title_value(&mut rendered, activity);
            remaining = placeholder;
        } else if let Some(placeholder) = remaining.strip_prefix("{bell}") {
            push_tab_title_value(&mut rendered, bell);
            remaining = placeholder;
        } else if let Some(placeholder) = remaining.strip_prefix("{program}") {
            remaining = placeholder;
        } else if let Some(placeholder) = remaining.strip_prefix("{cwd}") {
            remaining = placeholder;
        } else if let Some(placeholder) = remaining.strip_prefix("{modified}") {
            remaining = placeholder;
        } else {
            let ch = remaining.chars().next().unwrap();
            if !ch.is_control() {
                rendered.push(ch);
            }
            remaining = &remaining[ch.len_utf8()..];
        }
    }

    rendered
}

fn push_tab_title_value(title: &mut String, value: &str) {
    title.extend(value.chars().filter(|c| !c.is_control()));
}

fn tab_label_from_parts(
    tab_bar: &TabBarConfig,
    index: usize,
    active: bool,
    title: &str,
    has_activity: bool,
    has_bell: bool,
) -> String {
    let show_indicator = tab_bar.show_activity_indicator && !active;
    let activity =
        if show_indicator && has_activity { tab_bar.activity_indicator.as_str() } else { "" };
    let bell = if show_indicator && has_bell { tab_bar.bell_indicator.as_str() } else { "" };
    let template = if active {
        tab_bar.active_title_template.as_ref().or(tab_bar.title_template.as_ref())
    } else {
        tab_bar.inactive_title_template.as_ref().or(tab_bar.title_template.as_ref())
    };

    match template {
        Some(template) => {
            let label = format_tab_title(template, index, title, activity, bell);
            if show_indicator && !template.contains("{activity}") && !template.contains("{bell}") {
                if has_bell {
                    format!("{} {}", tab_bar.bell_indicator, label)
                } else if has_activity {
                    format!("{} {}", tab_bar.activity_indicator, label)
                } else {
                    label
                }
            } else {
                label
            }
        },
        None if show_indicator && has_bell => format!("{} {}", tab_bar.bell_indicator, title),
        None if show_indicator && has_activity => {
            format!("{} {}", tab_bar.activity_indicator, title)
        },
        None => title.to_owned(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabBarHit {
    Tab(TabId),
    Close(TabId),
    Background,
}

impl TabBarHit {
    fn tab_id(self) -> Option<TabId> {
        match self {
            Self::Tab(tab_id) | Self::Close(tab_id) => Some(tab_id),
            Self::Background => None,
        }
    }
}

/// Terminal session state associated with a window.
struct TerminalSession {
    session_ids: SessionIds,
    terminal: Arc<FairMutex<Term<EventProxy>>>,
    title: Option<String>,
    static_title: String,
    has_activity: bool,
    has_bell: bool,
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
    hold: bool,
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
        session_ids: SessionIds,
    ) -> Result<Self, Box<dyn Error>> {
        let mut pty_config = config.pty_config();
        options.terminal_options.override_pty_config(&mut pty_config);

        let preserve_title = options.window_identity.title.is_some();
        let mut identity = config.window.identity.clone();
        options.window_identity.override_identity_config(&mut identity);
        let static_title = identity.title;
        let hold = options.terminal_options.hold;

        info!(
            "PTY dimensions: {:?} x {:?}",
            display.size_info.screen_lines(),
            display.size_info.columns()
        );

        let event_proxy = EventProxy::new(
            proxy,
            display.window.id(),
            session_ids.tab_id,
            session_ids.terminal_id,
        );

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
            session_ids,
            terminal,
            title: None,
            static_title,
            has_activity: false,
            has_bell: false,
            #[cfg(not(windows))]
            master_fd,
            #[cfg(not(windows))]
            shell_pid,
            hold,
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
        if !self.preserve_title {
            self.static_title.clone_from(&self.config.window.identity.title);
        }

        old_config
    }

    fn terminal_id(&self) -> TerminalId {
        self.session_ids.terminal_id
    }

    fn set_title(&mut self, title: Option<String>) {
        self.title = title;
    }

    fn visible_title(&self) -> String {
        visible_title(
            &self.static_title,
            self.config.window.dynamic_title,
            self.preserve_title,
            self.title.as_deref(),
        )
    }

    fn mark_activity(&mut self) -> bool {
        let changed = !self.has_activity;
        self.has_activity = true;
        changed
    }

    fn mark_bell(&mut self) -> bool {
        let changed = !self.has_activity || !self.has_bell;
        self.has_bell = true;
        self.has_activity = true;
        changed
    }

    fn clear_activity(&mut self) {
        self.has_activity = false;
        self.has_bell = false;
    }

    fn tab_label(&self, index: usize, active: bool, tab_bar: &TabBarConfig) -> String {
        tab_label_from_parts(
            tab_bar,
            index,
            active,
            &self.visible_title(),
            self.has_activity,
            self.has_bell,
        )
    }

    fn spawn_daemon<I, S>(&self, program: &str, args: I)
    where
        I: IntoIterator<Item = S> + std::fmt::Debug + Copy,
        S: AsRef<std::ffi::OsStr>,
    {
        #[cfg(not(windows))]
        let result = crate::daemon::spawn_daemon(program, args, self.master_fd, self.shell_pid);
        #[cfg(windows)]
        let result = crate::daemon::spawn_daemon(program, args);

        if let Err(err) = result {
            log::warn!("Unable to launch {program} with args {args:?}: {err}");
        }
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
    tab_bar_mouse_grab: bool,
    #[cfg(not(any(target_os = "macos", windows)))]
    pending_activation_opens: Vec<PendingActivationOpen>,
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
        session_ids: SessionIds,
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

        Self::new(display, config, options, proxy, session_ids)
    }

    /// Create additional context with the graphics platform other windows are using.
    pub fn additional(
        gl_config: &GlutinConfig,
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<Event>,
        config: Rc<UiConfig>,
        mut options: WindowOptions,
        config_overrides: ParsedOptions,
        session_ids: SessionIds,
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

        let mut window_context = Self::new(display, config, options, proxy, session_ids)?;

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
        session_ids: SessionIds,
    ) -> Result<Self, Box<dyn Error>> {
        let session = TerminalSession::new(&display, config, &options, proxy, session_ids)?;
        let mut tabs = TabManager::new();
        tabs.open_with_id(session_ids.tab_id, session);

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
            tab_bar_mouse_grab: Default::default(),
            #[cfg(not(any(target_os = "macos", windows)))]
            pending_activation_opens: Default::default(),
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

        if old_config.window.tab_bar != config.window.tab_bar
            || old_config.colors.tab_bar != config.colors.tab_bar
        {
            self.display.damage_tracker.frame().mark_fully_damaged();
            self.display.pending_update.dirty = true;
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
            let title = self.active_session().visible_title();
            self.display.window.set_title(title);
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

    #[cfg(not(any(target_os = "macos", windows)))]
    pub fn take_pending_activation_open(
        &mut self,
        serial: AsyncRequestSerial,
    ) -> Option<PendingActivationOpen> {
        let index = self.pending_activation_opens.iter().position(|pending| {
            matches!(pending.request, ActivationOpenRequest::Winit(request) if request == serial)
        })?;
        Some(self.pending_activation_opens.remove(index))
    }

    #[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
    pub fn take_pending_serial_activation_open(&mut self) -> Option<PendingActivationOpen> {
        let index = self
            .pending_activation_opens
            .iter()
            .position(|pending| matches!(pending.request, ActivationOpenRequest::WaylandSerial))?;
        Some(self.pending_activation_opens.remove(index))
    }

    #[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
    pub fn has_pending_serial_activation_open(&self) -> bool {
        self.pending_activation_opens
            .iter()
            .any(|pending| matches!(pending.request, ActivationOpenRequest::WaylandSerial))
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    pub fn has_pending_activation_opens(&self) -> bool {
        !self.pending_activation_opens.is_empty()
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    pub fn drain_pending_activation_opens(&mut self) -> Vec<PendingActivationOpen> {
        std::mem::take(&mut self.pending_activation_opens)
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
        session_ids: SessionIds,
    ) -> Result<(), Box<dyn Error>> {
        let focused = self.active_session().terminal.lock().is_focused;
        let mut config_overrides = options.config_overrides();
        let mut config = self.active_session().config.clone();
        config = config_overrides.override_config_rc(config);

        let mut session =
            TerminalSession::new(&self.display, config, &options, proxy, session_ids)?;
        session.window_config = config_overrides;
        session.terminal.lock().is_focused = focused;

        if let Some(active_id) = self.tabs.active_id() {
            self.tabs.get_mut(active_id).unwrap().value_mut().terminal.lock().is_focused = false;
        }

        self.tabs.open_with_id(session_ids.tab_id, session);
        let (title, hold) = {
            let session = self.active_session();
            (session.visible_title(), session.hold)
        };
        self.display.window.hold = hold;
        self.display.window.set_title(title);
        self.mark_tab_change_dirty();

        Ok(())
    }

    pub fn select_tab(&mut self, selection: TabSelection) {
        let _ = self.try_select_tab(selection);
    }

    pub fn try_select_tab(&mut self, selection: TabSelection) -> Result<(), TabError> {
        let old_active_id = match self.tabs.active_id() {
            Some(active_id) => active_id,
            None => return Err(TabError::Empty),
        };
        let focused = self.active_session().terminal.lock().is_focused;

        self.tabs.select(selection)?;
        if self.tabs.active_id() == Some(old_active_id) {
            return Ok(());
        }

        self.tabs.get_mut(old_active_id).unwrap().value_mut().terminal.lock().is_focused = false;
        let (title, hold) = {
            let session = self.active_session_mut();
            session.clear_activity();
            session.terminal.lock().is_focused = focused;
            (session.visible_title(), session.hold)
        };
        self.display.window.hold = hold;
        self.display.window.set_title(title);
        self.mark_tab_change_dirty();

        Ok(())
    }

    pub fn selected_tab_id(&self, selection: TabSelection) -> Result<TabId, TabError> {
        self.tabs.selected_id(selection)
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
        if self.tab_bar.hovered_tab == Some(tab_id) {
            self.tab_bar.hovered_tab = None;
        }

        if was_active {
            let (title, hold) = {
                let session = self.active_session_mut();
                session.clear_activity();
                session.terminal.lock().is_focused = focused;
                (session.visible_title(), session.hold)
            };
            self.display.window.hold = hold;
            self.display.window.set_title(title);
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

    pub fn move_tab(&mut self, selection: TabSelection, index: usize) -> Result<TabId, TabError> {
        let tab_id = self.tabs.move_selection(selection, index)?;
        self.mark_tab_change_dirty();
        Ok(tab_id)
    }

    pub fn move_active_tab_left(&mut self) -> bool {
        let Some(index) = self.tabs.active_index() else {
            return false;
        };

        index > 0 && self.move_tab(TabSelection::Active, index - 1).is_ok()
    }

    pub fn move_active_tab_right(&mut self) -> bool {
        let Some(index) = self.tabs.active_index() else {
            return false;
        };

        index + 1 < self.tabs.len() && self.move_tab(TabSelection::Active, index + 1).is_ok()
    }

    #[cfg(unix)]
    pub fn tab_infos(&self) -> Vec<TabInfo> {
        let active_tab_id = self.tabs.active_id();
        self.tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| TabInfo {
                id: tab.id().as_u64(),
                index: index as u64,
                active: active_tab_id == Some(tab.id()),
                title: tab.value().visible_title(),
            })
            .collect()
    }

    #[cfg(unix)]
    pub fn active_tab_info(&self) -> Option<TabInfo> {
        let active_tab_id = self.tabs.active_id();
        self.tabs.iter().enumerate().find_map(|(index, tab)| {
            (Some(tab.id()) == active_tab_id).then(|| TabInfo {
                id: tab.id().as_u64(),
                index: index as u64,
                active: true,
                title: tab.value().visible_title(),
            })
        })
    }

    #[cfg(unix)]
    pub fn is_focused(&self) -> bool {
        self.active_session().terminal.lock().is_focused
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

    fn session_by_ids_mut(
        &mut self,
        tab_id: Option<TabId>,
        terminal_id: Option<TerminalId>,
    ) -> Option<&mut TerminalSession> {
        match (tab_id, terminal_id) {
            (Some(tab_id), Some(terminal_id)) => {
                let session = self.tabs.get_mut(tab_id)?;
                (session.value().terminal_id() == terminal_id).then_some(session.value_mut())
            },
            (Some(tab_id), None) => self.tabs.get_mut(tab_id).map(Tab::value_mut),
            (None, Some(terminal_id)) => self
                .tabs
                .iter_mut()
                .find(|tab| tab.value().terminal_id() == terminal_id)
                .map(Tab::value_mut),
            (None, None) => Some(self.active_session_mut()),
        }
    }

    fn validate_terminal_event_target(
        &mut self,
        tab_id: Option<TabId>,
        terminal_id: Option<TerminalId>,
    ) -> Option<&mut TerminalSession> {
        self.session_by_ids_mut(tab_id, terminal_id)
    }

    pub fn terminal_event_target_valid(
        &self,
        tab_id: Option<TabId>,
        terminal_id: Option<TerminalId>,
    ) -> bool {
        match (tab_id, terminal_id) {
            (Some(tab_id), Some(terminal_id)) => self
                .tabs
                .get(tab_id)
                .is_some_and(|session| session.value().terminal_id() == terminal_id),
            (Some(tab_id), None) => self.has_tab(tab_id),
            (None, Some(terminal_id)) => {
                self.tabs.iter().any(|tab| tab.value().terminal_id() == terminal_id)
            },
            (None, None) => true,
        }
    }

    pub fn tab_holds_on_exit(&self, tab_id: TabId) -> Option<bool> {
        self.tabs.get(tab_id).map(|tab| tab.value().hold)
    }

    pub fn update_terminal_title(
        &mut self,
        tab_id: Option<TabId>,
        terminal_id: Option<TerminalId>,
        title: Option<String>,
    ) -> bool {
        let Some(session) = self.validate_terminal_event_target(tab_id, terminal_id) else {
            return false;
        };

        session.set_title(title);
        self.mark_tab_change_dirty();

        true
    }

    pub fn handle_terminal_event(
        &mut self,
        tab_id: Option<TabId>,
        terminal_id: Option<TerminalId>,
        event: TerminalEvent,
        clipboard: &mut Clipboard,
        _scheduler: &mut Scheduler,
    ) -> bool {
        let active = self.is_active_tab(tab_id);
        match event {
            TerminalEvent::Title(title) => {
                let window_title = {
                    let Some(session) = self.validate_terminal_event_target(tab_id, terminal_id)
                    else {
                        return false;
                    };
                    session.set_title(Some(title.clone()));
                    if active { Some(session.visible_title()) } else { None }
                };
                if let Some(title) = window_title {
                    self.display.window.set_title(title);
                }
                self.mark_tab_change_dirty();
            },
            TerminalEvent::ResetTitle => {
                let window_title = {
                    let Some(session) = self.validate_terminal_event_target(tab_id, terminal_id)
                    else {
                        return false;
                    };
                    session.set_title(None);
                    if active { Some(session.visible_title()) } else { None }
                };
                if let Some(title) = window_title {
                    self.display.window.set_title(title);
                }
                self.mark_tab_change_dirty();
            },
            TerminalEvent::Bell => {
                const BELL_CMD_COOLDOWN: std::time::Duration =
                    std::time::Duration::from_millis(100);
                let mut urgent = false;
                let mut ring = false;
                let mut tab_activity_changed = false;
                {
                    let Some(session) = self.validate_terminal_event_target(tab_id, terminal_id)
                    else {
                        return false;
                    };
                    if !active {
                        tab_activity_changed = session.mark_bell();
                    }
                    if active {
                        let terminal = session.terminal.lock();
                        urgent = terminal.mode().contains(TermMode::URGENCY_HINTS)
                            && !terminal.is_focused;
                        ring = true;
                    }
                    if let Some(bell_command) = &session.config.bell.command
                        && session.prev_bell_cmd.is_none_or(|i| i.elapsed() >= BELL_CMD_COOLDOWN)
                    {
                        session.spawn_daemon(bell_command.program(), bell_command.args());
                        session.prev_bell_cmd = Some(Instant::now());
                    }
                }
                if urgent {
                    self.display.window.set_urgent(true);
                }
                if ring {
                    self.display.visual_bell.ring();
                }
                if tab_activity_changed {
                    self.mark_tab_change_dirty();
                    if self.display.window.has_frame {
                        self.display.window.request_redraw();
                    }
                }
            },
            TerminalEvent::ClipboardStore(clipboard_type, content) => {
                let focused = {
                    let Some(session) = self.validate_terminal_event_target(tab_id, terminal_id)
                    else {
                        return false;
                    };
                    active && session.terminal.lock().is_focused
                };
                if focused {
                    clipboard.store(clipboard_type, content);
                }
            },
            TerminalEvent::ClipboardLoad(clipboard_type, format) => {
                let focused = {
                    let Some(session) = self.validate_terminal_event_target(tab_id, terminal_id)
                    else {
                        return false;
                    };
                    active && session.terminal.lock().is_focused
                };
                if focused {
                    let text = format(clipboard.load(clipboard_type).as_str());
                    if let Some(session) = self.validate_terminal_event_target(tab_id, terminal_id)
                    {
                        session.notifier.notify(text.into_bytes());
                    }
                }
            },
            TerminalEvent::ColorRequest(index, format) => {
                let default_color = self.display.colors[index];
                let color = {
                    let Some(session) = self.validate_terminal_event_target(tab_id, terminal_id)
                    else {
                        return false;
                    };
                    let terminal = session.terminal.lock();
                    match terminal.colors()[index] {
                        Some(color) => crate::display::color::Rgb(color),
                        None if index == NamedColor::Cursor as usize => return true,
                        None => default_color,
                    }
                };
                if let Some(session) = self.validate_terminal_event_target(tab_id, terminal_id) {
                    session.notifier.notify(format(color.0).into_bytes());
                }
            },
            TerminalEvent::TextAreaSizeRequest(format) => {
                let size = self.display.size_info;
                if let Some(session) = self.validate_terminal_event_target(tab_id, terminal_id) {
                    session.notifier.notify(format(size.into()).into_bytes());
                }
            },
            TerminalEvent::PtyWrite(text) => {
                if let Some(session) = self.validate_terminal_event_target(tab_id, terminal_id) {
                    session.notifier.notify(text.into_bytes());
                }
            },
            TerminalEvent::MouseCursorDirty => {
                if active {
                    self.dirty = true;
                }
            },
            TerminalEvent::CursorBlinkingChange => {
                if active {
                    self.display.pending_update.set_cursor_dirty();
                }
            },
            TerminalEvent::Wakeup => {
                let tab_activity_changed =
                    match self.validate_terminal_event_target(tab_id, terminal_id) {
                        Some(session) if !active => session.mark_activity(),
                        Some(_) => false,
                        None => return false,
                    };
                if tab_activity_changed {
                    self.mark_tab_change_dirty();
                }
                if (active || tab_activity_changed) && self.display.window.has_frame {
                    self.dirty = true;
                    self.display.window.request_redraw();
                }
            },
            TerminalEvent::Exit | TerminalEvent::ChildExit(_) => {},
        }

        true
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

    fn resize_inactive_sessions(&mut self) {
        let active_id = self.tabs.active_id();
        let size = self.display.size_info;
        for tab in self.tabs.iter_mut().filter(|tab| Some(tab.id()) != active_id) {
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

    fn tab_bar_visible(&self, config: &UiConfig) -> bool {
        if cfg!(target_os = "macos") {
            return false;
        }

        match config.window.tab_bar.visibility {
            TabBarVisibility::Auto => self.tabs.len() >= 2,
            TabBarVisibility::Always => true,
            TabBarVisibility::Never => false,
        }
    }

    fn tab_bar_entries(&self) -> Vec<crate::display::TabBarEntry> {
        let active_tab_id = self.tabs.active_id();
        let tab_bar = &self.active_session().config.window.tab_bar;
        self.tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| crate::display::TabBarEntry {
                id: tab.id(),
                index,
                label: tab.value().tab_label(index, active_tab_id == Some(tab.id()), tab_bar),
                active: active_tab_id == Some(tab.id()),
            })
            .collect()
    }

    fn tab_bar_layout(
        &self,
        entries: &[crate::display::TabBarEntry],
    ) -> crate::display::TabBarLayout {
        let size = self.display.size_info;
        let config = &self.active_session().config;
        let tab_config = &config.window.tab_bar;
        let visibility = if self.tab_bar_visible(config) {
            tab_config.visibility.into()
        } else {
            crate::display::TabBarVisibility::Never
        };

        let search_lines = usize::from(self.active_session().search_state.regex().is_some());
        let message_lines = self.message_buffer.message().map_or(0, |m| m.text(&size).len());
        crate::display::layout_tab_bar(crate::display::TabBarLayoutInput {
            tabs: entries,
            columns: size.columns(),
            screen_lines: size.screen_lines(),
            search_lines,
            message_lines,
            visibility,
            position: tab_config.position.into(),
            alignment: tab_config.alignment.into(),
            close_button_visibility: tab_config.close_button.into(),
            hovered_tab: self.tab_bar.hovered_tab,
            show_indices: tab_config.show_indices(),
            max_width: Some(tab_config.max_width),
            min_width: tab_config.min_width,
        })
    }

    fn tab_bar_hit_test(&self) -> Option<TabBarHit> {
        let entries = self.tab_bar_entries();
        let layout = self.tab_bar_layout(&entries);
        if !layout.visible {
            return None;
        }

        let size = self.display.size_info;
        let tab_bar_line = layout.row?;
        let tab_bar_y = size.cell_height().mul_add(tab_bar_line as f32, size.padding_y()) as usize;
        if !(self.mouse.y >= tab_bar_y && self.mouse.y < tab_bar_y + size.cell_height() as usize) {
            return None;
        }

        if self.mouse.x < size.padding_x() as usize
            || self.mouse.x
                >= (size.padding_x() + size.columns() as f32 * size.cell_width()) as usize
        {
            return Some(TabBarHit::Background);
        }

        let x = (self.mouse.x as f32 - size.padding_x()) / size.cell_width();
        if let Some((tab_id, ..)) = layout
            .close_regions
            .iter()
            .find(|(_, start, end)| (x as usize) >= *start && (x as usize) < *end)
        {
            return Some(TabBarHit::Close(*tab_id));
        }

        layout
            .hit_regions
            .iter()
            .find(|(_, start, end)| (x as usize) >= *start && (x as usize) < *end)
            .map(|(tab_id, ..)| TabBarHit::Tab(*tab_id))
            .or(Some(TabBarHit::Background))
    }

    fn update_mouse_button_state(&mut self, button: MouseButton, state: ElementState) {
        match button {
            MouseButton::Left => self.mouse.left_button_state = state,
            MouseButton::Middle => self.mouse.middle_button_state = state,
            MouseButton::Right => self.mouse.right_button_state = state,
            _ => (),
        }
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
        let tab_bar = &self.active_session().config.window.tab_bar;
        self.tab_bar.tabs = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| crate::display::TabBarEntry {
                id: tab.id(),
                index,
                label: tab.value().tab_label(index, active_tab_id == Some(tab.id()), tab_bar),
                active: active_tab_id == Some(tab.id()),
            })
            .collect();
        self.tab_bar.hit_regions.clear();
        self.tab_bar.close_regions.clear();
        self.tab_bar.row = None;

        let session = self.tabs.active_mut().expect("window has an active tab").value_mut();
        let terminal = session.terminal.lock();
        let tab_bar = if cfg!(target_os = "macos") { None } else { Some(&mut self.tab_bar) };
        self.display.draw(
            terminal,
            scheduler,
            &self.message_buffer,
            &session.config,
            &mut session.search_state,
            tab_bar,
        );
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
                let tab_bar_hit = self.tab_bar_hit_test();
                let hovered_tab = tab_bar_hit.and_then(TabBarHit::tab_id);
                if self.tab_bar.hovered_tab != hovered_tab {
                    self.tab_bar.hovered_tab = hovered_tab;
                    self.mark_tab_change_dirty();
                }
                if self.tab_bar_mouse_grab || tab_bar_hit.is_some() {
                    self.mouse.inside_text_area = false;
                    return;
                }
                self.event_queue.push(event);
                return;
            },
            WinitEvent::WindowEvent {
                event: WindowEvent::MouseInput { state, button, .. },
                ..
            } if self.tab_bar_mouse_grab || self.tab_bar_hit_test().is_some() => {
                let hit = self.tab_bar_hit_test();
                self.update_mouse_button_state(button, state);
                self.tab_bar_mouse_grab = state == ElementState::Pressed;

                if state == ElementState::Pressed && button == MouseButton::Left {
                    match hit {
                        Some(TabBarHit::Tab(tab_id)) => self.select_tab(TabSelection::Id(tab_id)),
                        Some(TabBarHit::Close(tab_id)) => {
                            if !self.close_tab(tab_id) && self.is_active_tab(Some(tab_id)) {
                                self.close_window();
                            }
                        },
                        Some(TabBarHit::Background) | None => (),
                    }
                }
                return;
            },
            WinitEvent::WindowEvent { event: WindowEvent::MouseWheel { .. }, .. }
                if self.tab_bar_hit_test().is_some() =>
            {
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

        let mut resize_inactive_sessions = false;
        let is_redraw =
            matches!(event, WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. });
        let request_redraw;
        {
            let tab_bar_visible = self.tab_bar_visible(&self.active_session().config);
            let session = self.tabs.active_mut().expect("window has an active tab").value_mut();
            let mut terminal = session.terminal.lock();

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
                #[cfg(not(any(target_os = "macos", windows)))]
                pending_activation_opens: &mut self.pending_activation_opens,
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
                resize_inactive_sessions = Self::submit_display_update(
                    &mut terminal,
                    &mut self.display,
                    &mut session.notifier,
                    &self.message_buffer,
                    &mut session.search_state,
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

            request_redraw =
                self.dirty && self.display.window.has_frame && !self.occluded && !is_redraw;
        }

        if resize_inactive_sessions {
            self.resize_inactive_sessions();
        }

        // Don't call `request_redraw` when event is `RedrawRequested` since the `dirty` flag
        // represents the current frame, but redraw is for the next frame.
        if request_redraw {
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
        config: &UiConfig,
        tab_bar_visible: bool,
    ) -> bool {
        let old_is_searching = search_state.history_index.is_some();
        let old_size = display.size_info;

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
        let size_changed = display.size_info != old_size;

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

        size_changed
    }
}

#[cfg(test)]
mod tests {
    use super::{format_tab_title, tab_label_from_parts, visible_title};

    use crate::config::window::TabBarConfig;

    #[test]
    fn visible_title_uses_terminal_title_when_dynamic_titles_are_enabled() {
        assert_eq!(visible_title("Alacritty", true, false, Some("shell")), "shell");
    }

    #[test]
    fn visible_title_falls_back_to_static_title_without_terminal_title() {
        assert_eq!(visible_title("Alacritty", true, false, None), "Alacritty");
    }

    #[test]
    fn visible_title_ignores_terminal_title_when_dynamic_titles_are_disabled() {
        assert_eq!(visible_title("Alacritty", false, false, Some("shell")), "Alacritty");
    }

    #[test]
    fn visible_title_preserves_cli_title() {
        assert_eq!(visible_title("custom", true, true, Some("shell")), "custom");
    }

    #[test]
    fn tab_title_template_replaces_known_placeholders_once() {
        assert_eq!(
            format_tab_title(
                "{index}:{zero_index}:{title}:{activity}:{bell}:{program}:{cwd}:{modified}",
                2,
                "{bell}\n",
                "•",
                "!",
            ),
            "3:2:{bell}:•:!:::"
        );
    }

    #[test]
    fn tab_label_adds_default_activity_indicators_to_inactive_tabs() {
        let tab_bar = TabBarConfig::default();

        assert_eq!(tab_label_from_parts(&tab_bar, 0, false, "shell", false, false), "shell");
        assert_eq!(tab_label_from_parts(&tab_bar, 0, false, "shell", true, false), "• shell");
        assert_eq!(tab_label_from_parts(&tab_bar, 0, false, "shell", true, true), "! shell");
        assert_eq!(tab_label_from_parts(&tab_bar, 0, true, "shell", true, true), "shell");
    }

    #[test]
    fn tab_label_uses_templates_and_honors_indicator_toggle() {
        let mut tab_bar = TabBarConfig {
            title_template: Some("{index}: {title}".into()),
            inactive_title_template: Some("{bell}{activity}{title}".into()),
            ..Default::default()
        };

        assert_eq!(tab_label_from_parts(&tab_bar, 1, true, "shell", false, false), "2: shell");
        assert_eq!(tab_label_from_parts(&tab_bar, 1, false, "shell", true, true), "!•shell");

        tab_bar.show_activity_indicator = false;

        assert_eq!(tab_label_from_parts(&tab_bar, 1, false, "shell", true, true), "shell");
    }
}
