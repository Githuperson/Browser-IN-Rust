//! The application: state, event loop, and everything that mutates them.

use std::{
  collections::{HashMap, HashSet},
  path::PathBuf,
  sync::{
    atomic::{AtomicU8, Ordering},
    Arc, Mutex, RwLock,
  },
  time::{Duration, Instant},
};

use bir_core::{ipc::{RequestToken, TabId, UiCommand, UiEvent, WindowId}, site_settings::Permission, time,
  url::{self as birurl, OmniboxInput}, ProfilePaths, Settings};
use bir_ext::{
  background::{self, Alarm},
  bridge::{self, BridgeRequest},
  registry::ExtensionRegistry,
};
use bir_net::{blocker::{BlockerOptions, BlockerScriptCache}, engine::Blocker, lists::FilterListManager};
use bir_perf::{
  lifecycle::{LifecycleAction, LifecyclePolicy, LifecycleScheduler, TabActivity, TabLifecycle},
  memory::{MemorySampler, MemorySnapshot},
};
use tao::{
  event::{Event, WindowEvent},
  event_loop::{ControlFlow, EventLoop, EventLoopProxy, EventLoopWindowTarget},
  window::WindowBuilder,
};
use wry::{WebContext, WebView, WebViewBuilder};

use crate::{attach, commands::PageSignal, pages::PageHost, tab::Tab, window::BrowserWindow, Result};

/// Internal scheme for the chrome, internal pages and extension resources.
pub const BIR_SCHEME: &str = "bir";

/// The chrome document.
pub const CHROME_URL: &str = "bir://chrome";

/// How often background work (memory sampling, download progress, alarms) happens.
const TICK: Duration = Duration::from_millis(1000);

/// How often the session document is rewritten.
const SESSION_SAVE_INTERVAL: Duration = Duration::from_secs(30);

/// Bit flags shared with the (synchronous) navigation handler.
const FLAG_NETWORK: u8 = 1 << 0;
const FLAG_COSMETIC: u8 = 1 << 1;

/// Blocking configuration visible to webview callbacks.
///
/// The navigation handler has to answer synchronously, so it cannot take a lock on the
/// whole `Settings`. The two facts it needs are mirrored here as one atomic byte plus a
/// small set of excepted hosts.
pub struct BlockingState {
  flags: AtomicU8,
  /// Hosts where the user turned blocking off.
  exceptions: RwLock<HashSet<String>>,
}

impl BlockingState {
  pub fn new(network: bool, cosmetic: bool) -> Self {
    let mut flags = 0;
    if network {
      flags |= FLAG_NETWORK;
    }
    if cosmetic {
      flags |= FLAG_COSMETIC;
    }
    Self {
      flags: AtomicU8::new(flags),
      exceptions: RwLock::new(HashSet::new()),
    }
  }

  pub fn network_enabled(&self) -> bool {
    self.flags.load(Ordering::Relaxed) & FLAG_NETWORK != 0
  }

  pub fn cosmetic_enabled(&self) -> bool {
    self.flags.load(Ordering::Relaxed) & FLAG_COSMETIC != 0
  }

  pub fn set(&self, network: bool, cosmetic: bool) {
    let mut flags = 0;
    if network {
      flags |= FLAG_NETWORK;
    }
    if cosmetic {
      flags |= FLAG_COSMETIC;
    }
    self.flags.store(flags, Ordering::Relaxed);
  }

  pub fn is_excepted(&self, host: &str) -> bool {
    self
      .exceptions
      .read()
      .map(|set| set.contains(&host.to_ascii_lowercase()))
      .unwrap_or(false)
  }

  pub fn set_exception(&self, host: &str, excepted: bool) {
    if let Ok(mut set) = self.exceptions.write() {
      if excepted {
        set.insert(host.to_ascii_lowercase());
      } else {
        set.remove(&host.to_ascii_lowercase());
      }
    }
  }
}

/// Download plumbing shared with the (synchronous) download handlers.
#[derive(Default)]
pub struct DownloadCenter {
  pub dir: PathBuf,
  /// (url, chosen path) for downloads the webview has started.
  pub started: Vec<(String, PathBuf)>,
  /// (url, final path, success) for finished downloads.
  pub finished: Vec<(String, Option<PathBuf>, bool)>,
}

/// Everything that can be posted into the event loop.
pub enum AppEvent {
  /// A command from the chrome or from an internal page.
  UiCommand {
    window: WindowId,
    tab: Option<TabId>,
    command: UiCommand,
  },
  /// A navigation was allowed and is about to happen.
  Navigating {
    window: WindowId,
    tab: TabId,
    url: String,
  },
  /// A navigation was blocked by the content blocker.
  Blocked {
    window: WindowId,
    tab: TabId,
    url: String,
    rule: String,
  },
  PageLoad {
    window: WindowId,
    tab: TabId,
    started: bool,
    url: String,
  },
  Title {
    window: WindowId,
    tab: TabId,
    title: String,
  },
  /// An extension API call. `tab: None` means it came from the background host.
  ExtensionRequest {
    window: WindowId,
    tab: Option<TabId>,
    request: BridgeRequest,
  },
  MenuClicked {
    window: WindowId,
    tab: TabId,
    ext: String,
    menu_item_id: String,
    info: serde_json::Value,
  },
  /// A signal from the page-bridge script running inside a page.
  PageSignal {
    window: WindowId,
    tab: TabId,
    signal: PageSignal,
  },
  /// A webview asked for a device permission (camera, microphone, geolocation, ...).
  PermissionRequested {
    window: WindowId,
    tab: TabId,
    permission: Permission,
  },
  /// A favicon finished downloading.
  Favicon {
    window: WindowId,
    tab: TabId,
    data_url: String,
  },
  /// Result of a find-in-page query.
  FindResult {
    window: WindowId,
    tab: TabId,
    matches: usize,
    current: usize,
  },
  /// A message we could not classify; dropped on purpose.
  Ignored,
}

/// What the first window should show, decided by the CLI before the event loop starts.
#[derive(Debug, Clone, Default)]
pub struct Startup {
  /// URLs to open instead of the configured new-tab page.
  pub urls: Vec<String>,
  /// Open them in a private window.
  pub private: bool,
}

pub struct BrowserApp {
  pub paths: ProfilePaths,
  pub startup: Startup,
  pub settings: Settings,
  pub history: bir_core::HistoryStore,
  pub bookmarks: bir_core::BookmarkStore,
  pub downloads: bir_core::DownloadManager,
  pub session: bir_core::SessionStore,
  pub site_settings: bir_core::SiteSettings,
  pub engines: bir_core::SearchEngines,

  pub blocker: Arc<Blocker>,
  pub blocker_script: Option<BlockerScriptCache>,
  pub blocking: Arc<BlockingState>,

  pub extensions: ExtensionRegistry,
  /// Context-menu items declared by extensions, held outside the webviews so a reload
  /// can restore them.
  pub menus: crate::ext_api::MenuRegistry,
  pub alarms: Vec<Alarm>,

  pub windows: Vec<BrowserWindow>,
  web_context: WebContext,
  private_context: Option<WebContext>,
  /// Custom-protocol names already registered against each context.
  ///
  /// On Linux wry registers custom protocols **per WebContext** and fails if the same
  /// scheme is registered twice, so only the first webview in a context may declare it.
  registered: HashSet<String>,
  private_registered: HashSet<String>,
  /// Hidden webview hosting background scripts.
  pub background: Option<WebView>,
  background_window: WindowId,
  background_attempted: bool,

  pub scheduler: LifecycleScheduler,
  pub memory: MemorySampler,
  pub cpu: bir_perf::CpuSampler,
  pub last_memory: MemorySnapshot,
  /// Smoothed CPU usage of the browser process, percent of one core.
  pub cpu_percent: f32,

  pub download_center: Arc<Mutex<DownloadCenter>>,
  pub page_host: Arc<PageHost>,

  pub proxy: Option<EventLoopProxy<AppEvent>>,
  /// Current modifier state, tracked because `KeyEvent` does not carry it.
  pub modifiers: tao::keyboard::ModifiersState,
  pub(crate) next_tab_id: u64,
  next_window_id: u64,
  next_token: u64,
  pub pending_permissions: HashMap<RequestToken, (String, Permission, TabId)>,
  /// Webview permission decisions, keyed by permission name.
  ///
  /// wry's permission handler is synchronous and does not receive a URL, so decisions
  /// are global rather than per-origin. Anything the user chooses is mirrored into
  /// `site_settings` under the `*` origin so it survives a restart.
  pub permission_decisions: Arc<RwLock<HashMap<String, bool>>>,

  last_session_save: Instant,
  quitting: bool,
}

impl BrowserApp {
  // ------------------------------------------------------------------ startup

  pub fn new(paths: ProfilePaths, proxy: EventLoopProxy<AppEvent>) -> Result<Self> {
    paths.ensure()?;

    let settings = Settings::load(&paths)?;
    let history = bir_core::HistoryStore::load(&paths)?;
    let bookmarks = bir_core::BookmarkStore::load(&paths)?;
    let mut downloads = bir_core::DownloadManager::load(&paths)?;
    let session = bir_core::SessionStore::load(&paths)?;
    let site_settings = bir_core::SiteSettings::load(&paths)?;
    let engines = bir_core::SearchEngines::load(&paths)?;
    downloads.poll();

    // Content blocking: built-in lists ship with the binary, remote lists are read
    // from the cache and refreshed in the background by the tick handler.
    let lists = FilterListManager::new(&paths);
    let rules = lists.load(&settings.advanced.filter_lists)?;
    let blocker = Arc::new(Blocker::new(rules));

    let blocking = Arc::new(BlockingState::new(
      settings.privacy.block_ads || settings.privacy.block_trackers,
      settings.privacy.block_cosmetic,
    ));
    let blocker_script = Some(BlockerScriptCache::new(
      &blocker,
      &BlockerOptions {
        block_network: blocking.network_enabled(),
        block_cosmetic: blocking.cosmetic_enabled(),
        watch_dom: true,
      },
    ));

    let extensions = ExtensionRegistry::load(&paths)?;
    if cfg!(target_os = "windows") && settings.extensions.native_webview2_extensions {
      if let Err(err) = extensions.sync_native_dir() {
        eprintln!("[bir] could not mirror extensions for WebView2: {err}");
      }
    }

    // Mirror "blocking off for this site" choices into the sync-friendly state.
    for host in site_settings.known_hosts() {
      if !site_settings.allows(&host, Permission::ContentBlocking) {
        blocking.set_exception(&host, true);
      }
    }

    let download_center = Arc::new(Mutex::new(DownloadCenter {
      dir: settings
        .downloads
        .dir
        .clone()
        .unwrap_or_else(|| paths.downloads_dir().to_path_buf()),
      started: Vec::new(),
      finished: Vec::new(),
    }));

    // Load remembered permission decisions (stored against the "*" origin).
    let mut decisions: HashMap<String, bool> = HashMap::new();
    for permission in Permission::ALL {
      // Only *remembered* decisions are restored; an unset permission must still ask.
      if let Some(allowed) = site_settings.decision("*", *permission) {
        decisions.insert(format!("{permission:?}"), allowed);
      }
    }

    let page_host = Arc::new(PageHost::new(paths.extensions_dir()));
    let web_context = WebContext::new(Some(paths.webview_data_dir()));

    Ok(Self {
      paths,
      startup: Startup::default(),
      settings,
      history,
      bookmarks,
      downloads,
      session,
      site_settings,
      engines,
      blocker,
      blocker_script,
      blocking,
      extensions,
      menus: crate::ext_api::MenuRegistry::new(),
      alarms: Vec::new(),
      windows: Vec::new(),
      web_context,
      private_context: None,
      registered: HashSet::new(),
      private_registered: HashSet::new(),
      background: None,
      background_window: 0,
      background_attempted: false,
      scheduler: LifecycleScheduler::new(LifecyclePolicy::from_settings(&settings.performance)),
      memory: MemorySampler::new(),
      cpu: bir_perf::CpuSampler::new(),
      last_memory: MemorySnapshot::default(),
      download_center,
      page_host,
      proxy: Some(proxy),
      modifiers: tao::keyboard::ModifiersState::default(),
      next_tab_id: 1,
      next_window_id: 1,
      next_token: 1,
      pending_permissions: HashMap::new(),
      permission_decisions: Arc::new(RwLock::new(decisions)),
      last_session_save: Instant::now(),
      quitting: false,
    })
  }

  /// Apply GPU policy and run the event loop. Takes ownership of the app: everything
  /// after this point happens inside event-loop callbacks.
  pub fn run(mut self, event_loop: EventLoop<AppEvent>) {
    bir_perf::apply_gpu_policy(self.settings.performance.gpu);

    let startup = self.startup.clone();
    let crashed = bir_core::SessionStore::previous_run_crashed(&self.paths);
    let _ = bir_core::SessionStore::mark_running(&self.paths);

    let restore = match self.settings.general.startup {
      bir_core::settings::StartupBehavior::RestoreSession => {
        !(crashed && !self.settings.general.restore_after_crash)
      }
      _ => false,
    };
    // An explicit URL on the command line beats every startup policy.
    let start_urls: Vec<String> = if !startup.urls.is_empty() {
      startup.urls.clone()
    } else if restore {
      Vec::new()
    } else {
      vec![match self.settings.general.startup {
        bir_core::settings::StartupBehavior::OpenHome => self.settings.general.home_url.clone(),
        _ => self.settings.general.new_tab_url.clone(),
      }]
    };
    let private = startup.private;
    let session = self.session.clone();

    let mut booted = false;
    let mut pending_urls = start_urls;
    let mut pending_session = session;
    let mut last_tick = Instant::now();

    event_loop.run(move |event, target, control_flow| {
      *control_flow = ControlFlow::WaitUntil(Instant::now() + TICK);

      match event {
        Event::NewEvents(_) => {
          if !booted {
            booted = true;
            if !pending_session.windows.is_empty() {
              let snapshot = std::mem::take(&mut pending_session);
              let urls = std::mem::take(&mut pending_urls);
              if self.bootstrap(target, &snapshot).is_err() {
                let _ = self.bootstrap_urls(target, urls, private);
              } else if !urls.is_empty() {
                // Session restored *and* URLs on the command line: put the URLs in a
                // new window rather than throwing away the restored one.
                let _ = self.bootstrap_urls(target, urls, private);
              }
            } else {
              let urls = std::mem::take(&mut pending_urls);
              if let Err(err) = self.bootstrap_urls(target, urls, private) {
                eprintln!("[bir] failed to open the first window: {err}");
                self.quitting = true;
              }
            }
          }
        }

        Event::WindowEvent { window_id, event, .. } => {
          self.handle_window_event(window_id, event);
        }

        Event::UserEvent(app_event) => {
          self.handle_app_event(target, app_event);
        }

        Event::MainEventsCleared => {
          // MainEventsCleared fires after *every* batch of events, so the tick is
          // rate-limited here rather than inside `tick()`.
          if last_tick.elapsed() >= TICK {
            last_tick = Instant::now();
            self.tick();
          }
        }

        Event::LoopDestroyed => {
          self.shutdown();
        }

        _ => {}
      }

      if self.quitting {
        *control_flow = ControlFlow::Exit;
      }
    });
  }

  fn bootstrap_urls(
    &mut self,
    target: &EventLoopWindowTarget<AppEvent>,
    urls: Vec<String>,
    private: bool,
  ) -> Result<()> {
    let urls = if urls.is_empty() {
      vec![self.settings.general.new_tab_url.clone()]
    } else {
      urls
    };
    let id = self.create_window(target, None, private)?;
    for url in urls {
      self.open_tab(id, &url, true)?;
    }
    self.show_window(id);
    Ok(())
  }

  fn bootstrap(
    &mut self,
    target: &EventLoopWindowTarget<AppEvent>,
    snapshot: &bir_core::SessionStore,
  ) -> Result<()> {
    if snapshot.windows.is_empty() {
      return Err(crate::Error::Other("empty session".into()));
    }
    for window in &snapshot.windows {
      let id = self.create_window(target, Some(window), window.private)?;
      let tabs = window.tabs.clone();
      for tab in &tabs {
        let tab_id = self.open_tab(id, &tab.url, tab.active)?;
        if let Some(window_state) = self.windows.iter_mut().find(|w| w.id == id) {
          if let Some(tab_state) = window_state.tab_mut(tab_id) {
            tab_state.title = tab.title.clone();
            tab_state.pinned = tab.pinned;
            tab_state.muted = tab.muted;
            tab_state.zoom = tab.zoom;
            if !tab.active && self.settings.performance.lazy_session_restore {
              // Restored but not shown: keep the record, skip the webview. This is the
              // difference between restoring 200 tabs instantly and restoring them at
              // all.
              tab_state.discard();
              tab_state.url = tab.url.clone();
              tab_state.restore_url = tab.url.clone();
            }
          }
        }
      }
      self.show_window(id);
    }
    Ok(())
  }

  fn show_window(&self, id: WindowId) {
    if let Some(window) = self.windows.iter().find(|w| w.id == id) {
      window.window.set_visible(true);
    }
  }

  // ------------------------------------------------------------------ windows

  /// Create a window plus its chrome webview.
  pub fn create_window(
    &mut self,
    target: &EventLoopWindowTarget<AppEvent>,
    snapshot: Option<&bir_core::session::WindowSnapshot>,
    private: bool,
  ) -> Result<WindowId> {
    let id = self.next_window_id;
    self.next_window_id += 1;

    let (width, height, x, y, maximized) = match snapshot {
      Some(s) => (s.width.max(520), s.height.max(380), s.x, s.y, s.maximized),
      None => (1280u32, 860u32, 60, 60, false),
    };

    let window = WindowBuilder::new()
      .with_title("BIR")
      .with_inner_size(tao::dpi::LogicalSize::new(width as f64, height as f64))
      .with_position(tao::dpi::LogicalPosition::new(x as f64, y as f64))
      .with_maximized(maximized)
      .with_visible(false)
      .build(target)
      .map_err(|e| crate::Error::Other(format!("could not create a window: {e}")))?;

    #[cfg(any(
      target_os = "linux",
      target_os = "dragonfly",
      target_os = "freebsd",
      target_os = "netbsd",
      target_os = "openbsd"
    ))]
    let container = attach::create_container(&window, width as i32, height as i32);

    let chrome = {
      #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
      ))]
      {
        self.build_chrome(id, private, attach::Surface::Gtk(&container))?
      }
      #[cfg(not(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
      )))]
      {
        self.build_chrome(id, private, attach::Surface::Window(&window))?
      }
    };

    let browser_window = BrowserWindow::new(
      id,
      window,
      chrome,
      private,
      self.settings.appearance.tab_layout == bir_core::settings::TabLayout::Vertical,
      #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
      ))]
      container,
    );
    self.windows.push(browser_window);
    self.relayout(id);
    Ok(id)
  }

  /// Build the HTML chrome webview for a window.
  fn build_chrome(
    &mut self,
    window_id: WindowId,
    _private: bool,
    surface: attach::Surface<'_>,
  ) -> Result<WebView> {
    let scheme = BIR_SCHEME.to_string();
    let register = !self.registered.contains(&scheme);
    let page_host = self.page_host.clone();
    let proxy = self.proxy.clone().expect("proxy is set in new()");
    let theme_js = crate::pages::theme_script(&self.settings);
    let devtools = self.settings.advanced.devtools;

    let builder = {
      let mut builder = WebViewBuilder::new_with_web_context(&mut self.web_context);
      if register {
        let host = page_host.clone();
        builder = builder.with_custom_protocol(scheme.clone(), move |_id, request| {
          host.serve(request)
        });
      }
      builder
        .with_url(CHROME_URL)
        .with_initialization_script(theme_js)
        .with_initialization_script(crate::pages::chrome_runtime_js())
        .with_bounds(
          attach::Frame {
            x: 0.0,
            y: 0.0,
            width: 1280.0,
            height: attach::CHROME_HEIGHT,
          }
          .to_wry(),
        )
        .with_clipboard(true)
        .with_devtools(devtools)
        .with_ipc_handler(move |request: wry::http::Request<String>| {
          let body = request.body().clone();
          if let Ok(command) = serde_json::from_str::<UiCommand>(&body) {
            let _ = proxy.send_event(AppEvent::UiCommand {
              window: window_id,
              tab: None,
              command,
            });
          }
        })
    };

    if register {
      self.registered.insert(scheme);
    }

    Ok(attach::build(builder, surface)?)
  }

  // -------------------------------------------------------------------- tabs

  /// Create a tab record and (unless lazy) its webview.
  pub fn open_tab(&mut self, window_id: WindowId, url: &str, foreground: bool) -> Result<TabId> {
    let id = self.next_tab_id;
    self.next_tab_id += 1;

    {
      let window = self
        .windows
        .iter_mut()
        .find(|w| w.id == window_id)
        .ok_or_else(|| crate::Error::Other("no such window".into()))?;
      window.push(Tab::new(id, url, false));
      if foreground {
        window.active = window.tabs.len() - 1;
        let index = window.active;
        window.tabs[index].last_active = time::now_secs();
      }
    }

    if foreground {
      self.ensure_webview(window_id, id)?;
      self.activate_tab(window_id, id);
    }
    self.push_tabs(window_id);
    Ok(id)
  }

  /// Create the webview for a tab if it does not have one yet.
  pub fn ensure_webview(&mut self, window_id: WindowId, tab_id: TabId) -> Result<()> {
    let url = match self.tab_ref(window_id, tab_id) {
      Some(tab) if tab.has_webview() => return Ok(()),
      Some(tab) => tab.effective_url().to_string(),
      None => return Err(crate::Error::Other("no such tab".into())),
    };

    let settings = self.settings.clone();
    let proxy = self.proxy.clone().expect("proxy");
    let page_host = self.page_host.clone();
    let blocker = self.blocker.clone();
    let blocking = self.blocking.clone();
    let downloads = self.download_center.clone();
    let scheme = BIR_SCHEME.to_string();
    let private = self
      .windows
      .iter()
      .find(|w| w.id == window_id)
      .map(|w| w.private)
      .unwrap_or(false);

    if private && self.private_context.is_none() {
      self.private_context = Some(WebContext::new(Some(
        self.paths.webview_data_dir().with_extension("private"),
      )));
    }

    let register = if private {
      !self.private_registered.contains(&scheme)
    } else {
      !self.registered.contains(&scheme)
    };

    let user_agent = settings.network.user_agent.clone();
    let allow_autoplay = settings.network.allow_autoplay;
    let devtools = settings.advanced.devtools;
    let gpu = settings.performance.gpu;
    let javascript = settings.advanced.javascript;
    let extensions_enabled = settings.extensions.enabled;
    let native_extensions = settings.extensions.native_webview2_extensions
      && cfg!(target_os = "windows")
      && extensions_enabled;
    let extensions_dir = self.extensions.native_extensions_dir();

    let builder = {
      let context: &mut WebContext = if private {
        self.private_context.as_mut().expect("created above")
      } else {
        &mut self.web_context
      };

      let mut builder = WebViewBuilder::new_with_web_context(context);

      if register {
        let host = page_host.clone();
        builder = builder.with_custom_protocol(scheme.clone(), move |_id, request| {
          host.serve(request)
        });
      }

      // Document-start scripts: these survive navigations, unlike `evaluate_script`.
      // The bridge must come first: the next script reads `window.bir`.
      builder = builder
        .with_initialization_script(crate::pages::page_bridge_js())
        .with_initialization_script(format!("window.bir.tabId={tab_id};"))
        .with_initialization_script(bridge::EXTENSION_RUNTIME_JS)
        .with_initialization_script(bir_net::blocker::RUNTIME);

      if let Some(user_agent) = user_agent {
        builder = builder.with_user_agent(user_agent);
      }
      if !javascript {
        builder = builder.with_javascript_disabled();
      }

      builder = builder
        .with_url(url)
        .with_clipboard(true)
        .with_autoplay(allow_autoplay)
        .with_devtools(devtools)
        .with_visible(false);

      #[cfg(target_os = "windows")]
      {
        use wry::WebViewBuilderExtWindows;
        builder = builder.with_additional_browser_args(bir_perf::webview2_extra_args(
          gpu,
          allow_autoplay,
        ));
        if native_extensions {
          builder = builder
            .with_browser_extensions_enabled(true)
            .with_extensions_path(extensions_dir);
        }
      }

      builder
    };

    if register {
      if private {
        self.private_registered.insert(scheme);
      } else {
        self.registered.insert(scheme);
      }
    }

    // --- handlers ----------------------------------------------------------
    let nav_proxy = proxy.clone();
    let title_proxy = proxy.clone();
    let load_proxy = proxy.clone();
    let ipc_proxy = proxy.clone();
    let window_proxy = proxy.clone();

    let builder = builder
      .with_navigation_handler(move |url: String| -> bool {
        if url.starts_with("bir://") || url.starts_with("about:") || url.starts_with("data:") {
          return true;
        }
        let host = bir_core::url::UrlInfo::parse(&url)
          .map(|i| i.host)
          .unwrap_or_default();
        if blocking.network_enabled() && !blocking.is_excepted(&host) {
          if let bir_net::BlockDecision::Blocked { rule } = blocker.check_navigation(&url) {
            let _ = nav_proxy.send_event(AppEvent::Blocked {
              window: window_id,
              tab: tab_id,
              url,
              rule,
            });
            return false;
          }
        }
        let _ = nav_proxy.send_event(AppEvent::Navigating {
          window: window_id,
          tab: tab_id,
          url,
        });
        true
      })
      .with_document_title_changed_handler(move |title: String| {
        let _ = title_proxy.send_event(AppEvent::Title {
          window: window_id,
          tab: tab_id,
          title,
        });
      })
      .with_on_page_load_handler(move |event, url| {
        let started = matches!(event, wry::PageLoadEvent::Started);
        let _ = load_proxy.send_event(AppEvent::PageLoad {
          window: window_id,
          tab: tab_id,
          started,
          url,
        });
      })
      .with_new_window_req_handler(move |url: String, _features| {
        let _ = window_proxy.send_event(AppEvent::UiCommand {
          window: window_id,
          tab: Some(tab_id),
          command: UiCommand::NewTab {
            url: Some(url),
            foreground: true,
            after: Some(tab_id),
          },
        });
        wry::NewWindowResponse::Deny
      })
      .with_ipc_handler(move |request: wry::http::Request<String>| {
        let body = request.body().clone();
        let event = crate::commands::route_message(&body, window_id, Some(tab_id));
        let _ = ipc_proxy.send_event(event);
      })
      .with_permission_handler({
        let decisions = self.permission_decisions.clone();
        let permission_proxy = proxy.clone();
        move |kind: wry::PermissionKind| -> wry::PermissionResponse {
          let Some(permission) = permission_for(kind) else {
            return wry::PermissionResponse::Allow;
          };
          let name = format!("{permission:?}");
          if let Ok(map) = decisions.read() {
            if let Some(allowed) = map.get(&name) {
              return if *allowed {
                wry::PermissionResponse::Allow
              } else {
                wry::PermissionResponse::Deny
              };
            }
          }
          // No stored answer. The handler cannot block (it runs on the webview's own
          // thread, so blocking would freeze the page), and it cannot wait for the UI.
          // We deny once, ask the user, and reload the page if they allow: the second
          // request is then answered from the map.
          let _ = permission_proxy.send_event(AppEvent::PermissionRequested {
            window: window_id,
            tab: tab_id,
            permission,
          });
          wry::PermissionResponse::Deny
        }
      })
      .with_download_started_handler(move |url: String, path: &mut PathBuf| -> bool {
        let dir = match downloads.lock() {
          Ok(center) => center.dir.clone(),
          // A poisoned lock means something already panicked; refuse rather than write
          // to an unknown location.
          Err(_) => return false,
        };
        let name = crate::commands::filename_from_url(&url);
        *path = bir_core::downloads::unique_path(&dir, &name);
        if let Ok(mut center) = downloads.lock() {
          center.started.push((url, path.clone()));
        }
        true
      })
      .with_download_completed_handler(move |url: String, path: Option<PathBuf>, success: bool| {
        if let Ok(mut center) = downloads.lock() {
          center.finished.push((url, path, success));
        }
      });

    // --- attach ------------------------------------------------------------
    let webview = {
      let browser_window = self
        .windows
        .iter()
        .find(|w| w.id == window_id)
        .ok_or_else(|| crate::Error::Other("no such window".into()))?;
      let (width, height) = browser_window.logical_size();
      let (_, content) = attach::layout(width, height, browser_window.vertical_tabs);

      #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
      ))]
      let built = attach::build(
        builder.with_bounds(content.to_wry()),
        attach::Surface::Gtk(&browser_window.container),
      );

      #[cfg(not(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
      )))]
      let built = attach::build(
        builder.with_bounds(content.to_wry()),
        attach::Surface::Window(&browser_window.window),
      );

      built?
    };

    if let Some(window) = self.windows.iter_mut().find(|w| w.id == window_id) {
      if let Some(tab) = window.tab_mut(tab_id) {
        tab.webview = Some(webview);
        tab.lifecycle = TabLifecycle::Sleeping;
        tab.last_active = time::now_secs();
      }
    }
    Ok(())
  }

  /// Make `tab_id` the visible tab of its window, waking it if necessary.
  pub fn activate_tab(&mut self, window_id: WindowId, tab_id: TabId) {
    if let Err(err) = self.ensure_webview(window_id, tab_id) {
      eprintln!("[bir] could not wake tab {tab_id}: {err}");
    }

    let vertical = self
      .windows
      .iter()
      .find(|w| w.id == window_id)
      .map(|w| w.vertical_tabs)
      .unwrap_or(false);

    if let Some(window) = self.windows.iter_mut().find(|w| w.id == window_id) {
      let (width, height) = window.logical_size();
      let (_, content) = attach::layout(width, height, vertical);
      let frame = content.clamped((width, height)).to_wry();

      for tab in window.tabs.iter_mut() {
        let is_target = tab.id == tab_id;
        if let Some(webview) = &tab.webview {
          if is_target {
            let _ = webview.set_bounds(frame);
            let _ = webview.set_visible(true);
            let _ = webview.focus();
            tab.lifecycle = TabLifecycle::Active;
            tab.last_active = time::now_secs();
          } else if tab.lifecycle.is_mapped() {
            let _ = webview.set_visible(false);
            tab.lifecycle = TabLifecycle::Sleeping;
          }
        }
      }
      if let Some(index) = window.index_of(tab_id) {
        window.active = index;
      }
    }

    self.push_tabs(window_id);
  }

  fn tab_ref(&self, window_id: WindowId, tab_id: TabId) -> Option<&Tab> {
    self.windows.iter().find(|w| w.id == window_id)?.tab(tab_id)
  }

  /// Recompute webview geometry for a window.
  pub fn relayout(&self, window_id: WindowId) {
    let Some(window) = self.windows.iter().find(|w| w.id == window_id) else {
      return;
    };
    let (width, height) = window.logical_size();
    let (chrome, content) = attach::layout(width, height, window.vertical_tabs);

    let _ = window
      .chrome
      .set_bounds(chrome.clamped((width, height)).to_wry());

    let frame = content.clamped((width, height)).to_wry();
    for tab in &window.tabs {
      if let Some(webview) = &tab.webview {
        let _ = webview.set_bounds(frame);
      }
    }

    #[cfg(any(
      target_os = "linux",
      target_os = "dragonfly",
      target_os = "freebsd",
      target_os = "netbsd",
      target_os = "openbsd"
    ))]
    {
      use gtk::prelude::*;
      window
        .container
        .set_size_request(width.max(1.0) as i32, height.max(1.0) as i32);
    }
  }

  // ----------------------------------------------------------------- events

  fn handle_window_event(&mut self, window_id: tao::window::WindowId, event: WindowEvent) {
    let Some(id) = self
      .windows
      .iter()
      .find(|w| w.window.id() == window_id)
      .map(|w| w.id)
    else {
      return;
    };

    match event {
      WindowEvent::Resized(_) => self.relayout(id),
      WindowEvent::ModifiersChanged(state) => crate::shortcuts::modifiers_changed(self, state),
      WindowEvent::Moved(_) | WindowEvent::Focused(_) => {}
      WindowEvent::CloseRequested => self.close_window(id),
      WindowEvent::KeyboardInput { event, .. } => crate::shortcuts::handle(self, id, &event),
      WindowEvent::ThemeChanged(theme) => {
        let theme = match theme {
          tao::window::Theme::Dark => bir_core::settings::Theme::Dark,
          tao::window::Theme::Light => bir_core::settings::Theme::Light,
        };
        if self.settings.appearance.theme == bir_core::settings::Theme::System {
          self.push_event(id, UiEvent::Theme { theme });
        }
      }
      _ => {}
    }
  }

  pub fn handle_app_event(&mut self, target: &EventLoopWindowTarget<AppEvent>, event: AppEvent) {
    match event {
      AppEvent::UiCommand { window, tab, command } => {
        if let UiCommand::NewWindow { private } = &command {
          let private = *private;
          match self.create_window(target, None, private) {
            Ok(id) => {
              let url = self.settings.general.new_tab_url.clone();
              if let Err(err) = self.open_tab(id, &url, true) {
                eprintln!("[bir] could not open a tab in the new window: {err}");
              }
              self.show_window(id);
            }
            Err(err) => eprintln!("[bir] new window failed: {err}"),
          }
          return;
        }
        if matches!(command, UiCommand::Quit) {
          self.quitting = true;
          return;
        }
        crate::commands::handle(self, window, tab, command);
      }
      AppEvent::Navigating { window, tab, url } => self.on_navigating(window, tab, &url),
      AppEvent::Blocked { window, tab, url, rule } => self.on_blocked(window, tab, &url, &rule),
      AppEvent::PageLoad { window, tab, started, url } => {
        self.on_page_load(window, tab, started, &url)
      }
      AppEvent::Title { window, tab, title } => self.on_title(window, tab, &title),
      AppEvent::ExtensionRequest { window, tab, request } => {
        crate::ext_api::handle(self, window, tab, request)
      }
      AppEvent::MenuClicked { ext, menu_item_id, info, .. } => {
        let payload = serde_json::json!({ "menuItemId": menu_item_id, "info": info });
        self.dispatch_extension_event(&ext, "contextMenus.onClicked", &payload);
      }
      AppEvent::PageSignal { window, tab, signal } => {
        crate::commands::handle_page_signal(self, window, tab, signal)
      }
      AppEvent::PermissionRequested { window, tab, permission } => {
        let token = self.next_token();
        let origin = self
          .windows
          .iter()
          .find(|w| w.id == window)
          .and_then(|w| w.tab(tab))
          .map(|t| t.host())
          .unwrap_or_default();
        self
          .pending_permissions
          .insert(token, (origin.clone(), permission, tab));
        self.push_event(window, UiEvent::PermissionRequest { token, origin, permission });
      }
      AppEvent::Favicon { window, tab, data_url } => {
        if let Some(window_state) = self.windows.iter_mut().find(|w| w.id == window) {
          if let Some(tab_state) = window_state.tab_mut(tab) {
            tab_state.favicon = data_url.clone();
          }
        }
        self.push_event(window, UiEvent::Favicon { tab, data_url });
      }
      AppEvent::FindResult { window, tab, matches, current } => {
        self.push_event(window, UiEvent::FindResult { tab, matches, current });
      }
      AppEvent::Ignored => {}
    }
  }

  fn on_title(&mut self, window: WindowId, tab: TabId, title: &str) {
    if let Some(window_state) = self.windows.iter_mut().find(|w| w.id == window) {
      if let Some(tab_state) = window_state.tab_mut(tab) {
        tab_state.title = title.to_string();
      }
    }
    self.push_event(
      window,
      UiEvent::Title {
        tab,
        title: title.to_string(),
      },
    );
    if let Some(window_state) = self.windows.iter().find(|w| w.id == window) {
      let is_active = window_state.active_tab().map(|t| t.id) == Some(tab);
      if is_active {
        let suffix = if title.is_empty() {
          String::new()
        } else {
          format!("{title} — ")
        };
        window_state.window.set_title(&format!("{suffix}BIR"));
      }
    }
    self.push_tabs(window);
  }

  fn on_navigating(&mut self, window: WindowId, tab: TabId, url: &str) {
    // Tracking-parameter stripping and HTTPS upgrades happen before the request is
    // made, so the network never sees the original URL.
    let mut target = url.to_string();
    if self.settings.privacy.strip_tracking_params {
      let (cleaned, changed) = birurl::strip_tracking(&target);
      if changed {
        target = cleaned;
      }
    }
    if self.settings.privacy.https_only {
      if let Some(upgraded) = birurl::upgrade_to_https(&target) {
        target = upgraded;
      }
    }

    if target != url {
      if let Some(window_state) = self.windows.iter_mut().find(|w| w.id == window) {
        if let Some(tab_state) = window_state.tab_mut(tab) {
          tab_state.url = target.clone();
          tab_state.restore_url = target.clone();
          tab_state.eval(&format!(
            "location.replace({})",
            serde_json::json!(target)
          ));
        }
      }
      return;
    }

    if let Some(window_state) = self.windows.iter_mut().find(|w| w.id == window) {
      if let Some(tab_state) = window_state.tab_mut(tab) {
        tab_state.url = url.to_string();
        tab_state.restore_url = url.to_string();
        tab_state.loading = true;
        tab_state.favicon.clear();
        tab_state.audible = false;
      }
    }
    self.push_event(window, UiEvent::Url { tab, url: url.to_string() });
    self.push_event(window, UiEvent::Progress { tab, loading: true, progress: 0.15 });
  }

  fn on_blocked(&mut self, window: WindowId, tab: TabId, url: &str, rule: &str) {
    if self.settings.advanced.show_blocked_page {
      let page = crate::pages::blocked_page(url, rule);
      if let Some(window_state) = self.windows.iter_mut().find(|w| w.id == window) {
        if let Some(tab_state) = window_state.tab_mut(tab) {
          tab_state.eval(&format!(
            "document.open();document.write({});document.close();",
            serde_json::json!(page)
          ));
        }
      }
    } else {
      self.toast(&format!("Blocked a request to {}", host_of(url)));
    }
    let _ = rule;
  }

  fn on_page_load(&mut self, window: WindowId, tab: TabId, started: bool, url: &str) {
    let host = host_of(url);
    let block_script = if started && self.blocking.cosmetic_enabled() {
      self
        .blocker_script
        .as_ref()
        .map(|cache| cache.script_for(&self.blocker, &host))
        .unwrap_or_default()
    } else {
      String::new()
    };

    let injections = if self.settings.extensions.enabled {
      self.extensions.content_scripts_for(url)
    } else {
      Vec::new()
    };

    let zoom = self
      .site_settings
      .zoom(&host)
      .unwrap_or(self.settings.appearance.default_zoom);

    if let Some(window_state) = self.windows.iter_mut().find(|w| w.id == window) {
      if let Some(tab_state) = window_state.tab_mut(tab) {
        if started {
          tab_state.loading = true;
          if !block_script.is_empty() {
            tab_state.eval(&block_script);
          }
          for injection in &injections {
            if injection.run_at == bir_ext::manifest::RunAt::DocumentStart {
              for css in &injection.css {
                tab_state.eval(&bridge::encode_insert_css(css));
              }
              for js in &injection.js {
                tab_state.eval(&bridge::wrap_script(&injection.extension_id, js));
              }
            }
          }
        } else {
          tab_state.loading = false;
          if let Some(webview) = &tab_state.webview {
            tab_state.can_go_back = webview.can_go_back().unwrap_or(false);
            tab_state.can_go_forward = webview.can_go_forward().unwrap_or(false);
            if (zoom - 1.0).abs() > 0.001 {
              // `WebView::zoom` is a real page zoom (layout + text), unlike a CSS
              // transform, so the page's own responsive breakpoints still behave.
              let _ = webview.zoom(zoom);
            }
          }
          for injection in &injections {
            if injection.run_at != bir_ext::manifest::RunAt::DocumentStart {
              for css in &injection.css {
                tab_state.eval(&bridge::encode_insert_css(css));
              }
              for js in &injection.js {
                tab_state.eval(&bridge::wrap_script(&injection.extension_id, js));
              }
            }
          }
        }
      }
    }

    if !started && !url.starts_with("bir://") {
      let title = self
        .windows
        .iter()
        .find(|w| w.id == window)
        .and_then(|w| w.tab(tab))
        .map(|t| t.title.clone())
        .unwrap_or_default();
      self.history.record(url, &title, false);
    }

    self.push_event(
      window,
      UiEvent::Progress {
        tab,
        loading: started,
        progress: if started { 0.15 } else { 1.0 },
      },
    );
    self.push_tabs(window);
  }

  // -------------------------------------------------------------- background

  /// Periodic work: memory, lifecycle, downloads, alarms, persistence.
  pub(crate) fn tick(&mut self) {
    self.last_memory = self.memory.sample();
    self.cpu_percent = self.cpu.sample();
    self.run_lifecycle();
    self.drain_downloads();

    // Alarms are hosted in Rust so they survive a background-webview reload.
    let now = time::now_secs();
    let mut due: Vec<(String, serde_json::Value)> = Vec::new();
    for alarm in self.alarms.iter_mut() {
      if alarm.due(now) {
        due.push((alarm.extension_id.clone(), serde_json::json!({ "name": alarm.name })));
        alarm.advance(now);
      }
    }
    for (ext, payload) in due {
      self.dispatch_extension_event(&ext, "alarms.onAlarm", &payload);
    }
    self.alarms.retain(|a| a.scheduled_at > now || a.period_minutes.is_some());

    self.ensure_background();

    self.push_stats();

    if self.last_session_save.elapsed() >= SESSION_SAVE_INTERVAL {
      self.save_session();
      self.flush_stores();
      self.last_session_save = Instant::now();
    }
  }

  /// Live resource usage for the performance section of the settings page.
  fn push_stats(&self) {
    let Some(window) = self.windows.first() else {
      return;
    };
    let mut live = 0usize;
    let mut sleeping = 0usize;
    let mut discarded = 0usize;
    for window_state in &self.windows {
      for tab in &window_state.tabs {
        match tab.lifecycle {
          bir_perf::TabLifecycle::Active | bir_perf::TabLifecycle::Visible => live += 1,
          bir_perf::TabLifecycle::Sleeping => sleeping += 1,
          bir_perf::TabLifecycle::Discarded => discarded += 1,
        }
      }
    }
    window.send_to_all(&encode(&UiEvent::Stats {
      rss_mib: self.last_memory.process_rss_mib(),
      webview_mib: (live as u32) * (bir_perf::ESTIMATED_WEBVIEW_MIB as u32),
      system_used_percent: self.last_memory.used_percent.min(100),
      cpu_percent: self.cpu_percent,
      tabs_live: live,
      tabs_sleeping: sleeping,
      tabs_discarded: discarded,
    }));
  }

  fn drain_downloads(&mut self) {
    let mut started: Vec<(String, PathBuf)> = Vec::new();
    let mut finished: Vec<(String, Option<PathBuf>, bool)> = Vec::new();
    if let Ok(mut center) = self.download_center.lock() {
      started.extend(center.started.drain(..));
      finished.extend(center.finished.drain(..));
    }

    for (url, path) in started {
      let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
      let (id, chosen) = self.downloads.start(&url, Some(&name), "");
      self.downloads.began(&id, Some(chosen));
    }
    for (url, path, success) in finished {
      let id = self
        .downloads
        .items()
        .iter()
        .find(|i| i.url == url && !i.is_finished())
        .map(|i| i.id.clone());
      if let Some(id) = id {
        self.downloads.began(&id, path);
        self.downloads.finished(&id, success);
      }
    }
    self.downloads.poll();

    if !self.downloads.items().is_empty() {
      let items = self.downloads.items().to_vec();
      for window in &self.windows {
        window.send(&encode(&UiEvent::Downloads { items: items.clone() }));
      }
    }
  }

  /// Ask the lifecycle scheduler what to do, then do it.
  fn run_lifecycle(&mut self) {
    let mut activities: Vec<TabActivity> = Vec::new();
    for window in &self.windows {
      for tab in &window.tabs {
        activities.push(TabActivity {
          id: tab.id,
          lifecycle: tab.lifecycle,
          last_active_secs: tab.last_active,
          pinned: tab.pinned,
          audible: tab.audible,
          host: tab.host(),
        });
      }
    }

    let actions = self
      .scheduler
      .evaluate(&activities, &self.last_memory, time::now_secs());

    for action in actions {
      let Some((window_id, _)) = self.locate(match action {
        LifecycleAction::Sleep(tab_id) | LifecycleAction::Discard(tab_id) => tab_id,
      }) else {
        continue;
      };

      match action {
        LifecycleAction::Sleep(tab_id) => {
          if let Some(window) = self.windows.iter_mut().find(|w| w.id == window_id) {
            if let Some(tab) = window.tab_mut(tab_id) {
              if tab.lifecycle.is_mapped() || !tab.has_webview() {
                continue;
              }
              if let Some(webview) = &tab.webview {
                let _ = webview.set_visible(false);
              }
              tab.lifecycle = TabLifecycle::Sleeping;
            }
            self.push_tabs(window_id);
          }
        }
        LifecycleAction::Discard(tab_id) => {
          if let Some(window) = self.windows.iter_mut().find(|w| w.id == window_id) {
            let mut discarded = false;
            if let Some(tab) = window.tab_mut(tab_id) {
              if tab.lifecycle.is_mapped() || tab.pinned || tab.audible {
                continue;
              }
              tab.discard();
              discarded = true;
            }
            if discarded {
              self.push_tabs(window_id);
            }
          }
        }
      }
    }
  }

  /// Which window owns a tab.
  fn locate(&self, tab_id: TabId) -> Option<(WindowId, usize)> {
    for window in &self.windows {
      if let Some(index) = window.index_of(tab_id) {
        return Some((window.id, index));
      }
    }
    None
  }

  // ------------------------------------------------------------- UI plumbing

  pub fn push_event(&self, window_id: WindowId, event: UiEvent) {
    if let Some(window) = self.windows.iter().find(|w| w.id == window_id) {
      window.send_to_all(&encode(&event));
    }
  }

  pub fn push_tabs(&self, window_id: WindowId) {
    if let Some(window) = self.windows.iter().find(|w| w.id == window_id) {
      let active = window.active_tab().map(|t| t.id);
      let tabs = window.tab_views();
      window.send(&encode(&UiEvent::Tabs { tabs, active }));
    }
  }

  pub fn push_extensions(&self) {
    let views = self.extensions.views();
    for window in &self.windows {
      window.send(&encode(&UiEvent::Extensions { items: views.clone() }));
    }
  }

  pub fn toast(&self, text: &str) {
    for window in &self.windows {
      window.send_to_all(&encode(&UiEvent::Toast {
        text: text.to_string(),
        kind: bir_core::ipc::ToastKind::Info,
      }));
    }
  }

  /// Deliver an event to every context an extension is running in.
  pub fn dispatch_extension_event(&self, ext: &str, event: &str, payload: &serde_json::Value) {
    let js = bridge::encode_event(ext, event, payload);
    if let Some(background) = &self.background {
      let _ = background.evaluate_script(&js);
    }
    for window in &self.windows {
      for tab in &window.tabs {
        if let Some(webview) = &tab.webview {
          let _ = webview.evaluate_script(&js);
        }
      }
    }
  }

  /// Resolve an extension API reply to whichever context asked for it.
  pub(crate) fn reply_to(
    &self,
    window: WindowId,
    tab: Option<TabId>,
    js: &str,
  ) {
    if let Some(tab_id) = tab {
      if let Some(window_state) = self.windows.iter().find(|w| w.id == window) {
        if let Some(tab_state) = window_state.tab(tab_id) {
          tab_state.eval(js);
          return;
        }
      }
    }
    if let Some(background) = &self.background {
      let _ = background.evaluate_script(js);
    }
  }

  /// Create the hidden webview that hosts background scripts (once).
  pub(crate) fn ensure_background(&mut self) {
    if self.background_attempted || !self.settings.extensions.enabled {
      return;
    }
    let Some(host_window) = self.windows.first().map(|w| w.id) else {
      return;
    };
    self.background_attempted = true;

    let scheme = BIR_SCHEME.to_string();
    let register = !self.registered.contains(&scheme);
    let page_host = self.page_host.clone();
    let proxy = self.proxy.clone();

    let built = {
      let browser_window = match self.windows.first() {
        Some(window) => window,
        None => return,
      };

      let mut builder = WebViewBuilder::new_with_web_context(&mut self.web_context);
      if register {
        let host = page_host.clone();
        builder = builder.with_custom_protocol(scheme.clone(), move |_id, request| {
          host.serve(request)
        });
      }
      builder = builder
        .with_url(background::BACKGROUND_URL)
        .with_initialization_script(bridge::EXTENSION_RUNTIME_JS)
        .with_visible(false)
        .with_bounds(
          attach::Frame {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
          }
          .to_wry(),
        );

      let window_id = host_window;
      if let Some(proxy) = proxy {
        builder = builder.with_ipc_handler(move |request: wry::http::Request<String>| {
          let body = request.body().clone();
          if let Ok(request) = serde_json::from_str::<BridgeRequest>(&body) {
            let _ = proxy.send_event(AppEvent::ExtensionRequest {
              window: window_id,
              tab: None,
              request,
            });
          }
        });
      }

      #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
      ))]
      let built = attach::build(builder, attach::Surface::Gtk(&browser_window.container));

      #[cfg(not(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
      )))]
      let built = attach::build(builder, attach::Surface::Window(&browser_window.window));

      built
    };

    match built {
      Ok(webview) => {
        if register {
          self.registered.insert(scheme);
        }
        self.background = Some(webview);
        self.background_window = host_window;
        self.start_background_extensions();
      }
      Err(err) => eprintln!("[bir] could not start the extension host: {err}"),
    }
  }

  /// Register and run every enabled extension's background scripts.
  fn start_background_extensions(&mut self) {
    let enabled: Vec<(String, serde_json::Value, serde_json::Value, Vec<String>)> = self
      .extensions
      .enabled()
      .map(|e| {
        (
          e.id.clone(),
          self.extensions.manifest_json(&e.id),
          self.extensions.messages_json(&e.id),
          e.background_scripts(),
        )
      })
      .collect();

    let Some(background) = &self.background else {
      return;
    };
    for (id, manifest, messages, scripts) in enabled {
      let _ = background.evaluate_script(&background::encode_register(&id, &manifest, &messages));
      for script in scripts {
        let js = background::encode_run_background_script(&id, &script);
        if let Err(err) = background.evaluate_script(&js) {
          eprintln!("[bir] background script for {id} failed: {err}");
        }
      }
    }
  }

  /// Re-run an extension's background scripts after a reload.
  pub fn restart_extension(&mut self, id: &str) {
    let Some(background) = &self.background else {
      return;
    };
    let manifest = self.extensions.manifest_json(id);
    let messages = self.extensions.messages_json(id);
    let scripts = self
      .extensions
      .get(id)
      .map(|e| e.background_scripts())
      .unwrap_or_default();
    let _ = background.evaluate_script(&background::encode_register(id, &manifest, &messages));
    for script in scripts {
      let _ = background.evaluate_script(&background::encode_run_background_script(id, &script));
    }
  }

  // -------------------------------------------------------------- lifecycle

  pub fn close_window(&mut self, id: WindowId) {
    self.save_session();
    self.windows.retain(|w| w.id != id);
    if self.windows.is_empty() {
      self.quitting = true;
    }
  }

  pub(crate) fn save_session(&mut self) {
    let mut session = bir_core::SessionStore { windows: Vec::new() };
    for window in &self.windows {
      let physical = window.window.inner_size();
      let position = window.window.outer_position().ok();
      session.windows.push(bir_core::session::WindowSnapshot {
        tabs: window
          .tabs
          .iter()
          .enumerate()
          .map(|(index, tab)| bir_core::session::TabSnapshot {
            url: tab.effective_url().to_string(),
            title: tab.title.clone(),
            pinned: tab.pinned,
            muted: tab.muted,
            zoom: tab.zoom,
            active: index == window.active,
          })
          .collect(),
        active: window.active,
        width: physical.width,
        height: physical.height,
        x: position.map(|p| p.x).unwrap_or(60),
        y: position.map(|p| p.y).unwrap_or(60),
        maximized: window.window.is_maximized(),
        private: window.private,
      });
    }
    session.normalise();
    self.session = session;
    if let Err(err) = self.session.save_to(&self.paths) {
      eprintln!("[bir] could not save the session: {err}");
    }
  }

  pub(crate) fn flush_stores(&mut self) {
    if let Err(err) = self.history.flush(&self.paths) {
      eprintln!("[bir] history flush failed: {err}");
    }
    if self.bookmarks.is_dirty() {
      if let Err(err) = self.bookmarks.save_to(&self.paths) {
        eprintln!("[bir] bookmark save failed: {err}");
      }
      self.bookmarks.clear_dirty();
    }
    if self.site_settings.is_dirty() {
      let _ = bir_core::persist(&self.paths, &mut self.site_settings);
    }
  }

  pub(crate) fn shutdown(&mut self) {
    self.save_session();
    self.flush_stores();
    let _ = self.settings.save(&self.paths);
    let _ = self.engines.save_to(&self.paths);
    match self.settings.privacy.clear_data_on_exit {
      bir_core::settings::ClearOnExit::Nothing => {}
      bir_core::settings::ClearOnExit::History => self.history.clear(),
      bir_core::settings::ClearOnExit::CookiesAndStorage => self.clear_site_data(),
      bir_core::settings::ClearOnExit::Everything => {
        self.history.clear();
        self.clear_site_data();
        let _ = std::fs::remove_dir_all(self.paths.cache_dir());
      }
    }
    bir_core::SessionStore::clear_running(&self.paths);
  }

  fn clear_site_data(&self) {
    for window in &self.windows {
      for tab in &window.tabs {
        if let Some(webview) = &tab.webview {
          let _ = webview.clear_all_browsing_data();
        }
      }
    }
  }

  /// Turn omnibox text into a URL, exactly the way the address bar will.
  pub(crate) fn resolve_input(&self, text: &str) -> String {
    let keywords = self.engines.keywords();
    match birurl::classify(text, &keywords) {
      OmniboxInput::Url(url) => url.to_string(),
      OmniboxInput::Internal(page) => format!("bir://{page}"),
      OmniboxInput::Search(query) => self.engines.default_engine().build_url(&query),
      OmniboxInput::SearchWith { engine, query } => match self.engines.by_keyword(&engine) {
        Some(engine) => engine.build_url(&query),
        None => self.engines.default_engine().build_url(&query),
      },
    }
  }

  /// Allocate a permission-request token.
  pub(crate) fn next_token(&mut self) -> RequestToken {
    let token = self.next_token;
    self.next_token += 1;
    token
  }

  /// Window hosting the background webview, for replies that have no tab.
  pub(crate) fn background_window(&self) -> WindowId {
    self.background_window
  }
}

/// Map a webview permission request onto our own permission type.
///
/// `None` means "not something a user should be asked about" — fonts, window management,
/// pointer lock, automatic downloads and media keys are granted silently, because
/// prompting for them (or refusing them) breaks ordinary pages for no privacy gain.
fn permission_for(kind: wry::PermissionKind) -> Option<Permission> {
  match kind {
    wry::PermissionKind::Microphone => Some(Permission::Microphone),
    wry::PermissionKind::Camera => Some(Permission::Camera),
    wry::PermissionKind::Geolocation => Some(Permission::Geolocation),
    wry::PermissionKind::Notifications => Some(Permission::Notifications),
    wry::PermissionKind::ClipboardRead => Some(Permission::ClipboardRead),
    wry::PermissionKind::DisplayCapture => Some(Permission::ScreenCapture),
    wry::PermissionKind::Midi => Some(Permission::Midi),
    wry::PermissionKind::Sensors => Some(Permission::Sensors),
    _ => None,
  }
}

/// Encode a [`UiEvent`] as the JS the chrome evaluates.
pub fn encode(event: &UiEvent) -> String {
  match bir_core::ipc::encode_event(event) {
    Ok(js) => js,
    Err(err) => {
      eprintln!("[bir] could not encode UI event: {err}");
      String::new()
    }
  }
}

fn host_of(url: &str) -> String {
  bir_core::url::UrlInfo::parse(url)
    .map(|i| i.host)
    .unwrap_or_default()
}
