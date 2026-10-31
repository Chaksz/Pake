//! Ad blocking built on Brave's `adblock-rust` engine.
//!
//! Two layers run together:
//!
//! * **Network filtering** is native on Windows. A WebView2
//!   `WebResourceRequested` handler asks the engine about every subresource
//!   before it loads and answers blocked requests with an empty `403`.
//! * **Cosmetic filtering** is injected CSS on every platform: a small curated
//!   sheet at document start (before first paint) plus the hostname-specific
//!   selectors the engine derives from the filter lists once a page loads.
//!
//! Filter lists are where the real coverage lives. A built-in list keeps the
//! most common ad and tracker networks blocked offline; EasyList and
//! EasyPrivacy are downloaded once, cached in the app config directory, and
//! refreshed in the background so long-lived installs do not go stale.
//!
//! `PAKE_ADBLOCK_LISTS` can point at a directory holding `easylist.txt` and
//! `easyprivacy.txt` to override the download entirely, which keeps builds
//! reproducible in air-gapped environments.

use adblock::cosmetic_filter_cache::ProceduralOrActionFilter;
use adblock::lists::{FilterSet, ParseOptions};
use adblock::request::Request;
use adblock::Engine;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, RwLock,
};
use std::time::{Duration, SystemTime};
use tauri::{AppHandle, Manager};

#[cfg(target_os = "windows")]
use tauri::WebviewWindow;
#[cfg(target_os = "windows")]
use webview2_com::{
    take_pwstr, FrameCreatedEventHandler, FrameNavigationStartingEventHandler,
    Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2Frame2, ICoreWebView2NavigationStartingEventArgs,
        ICoreWebView2WebResourceRequest, ICoreWebView2WebResourceRequestedEventArgs,
        ICoreWebView2_2, ICoreWebView2_4, COREWEBVIEW2_WEB_RESOURCE_CONTEXT,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MANIFEST,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_OTHER,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST,
    },
    WebResourceRequestedEventHandler,
};
#[cfg(target_os = "windows")]
use windows_core::{Interface, PWSTR};

/// Curated fallback rules, used until the downloaded lists are available.
const DEFAULT_ADBLOCK_RULES: &str = include_str!("adblock_defaults.txt");
/// Selectors injected on every page before first paint.
const GENERIC_COSMETIC_CSS: &str = include_str!("adblock_cosmetic.css");

const CACHE_SUBDIR: &str = "adblock";
const LIST_FILES: [&str; 2] = ["easylist.txt", "easyprivacy.txt"];
const LIST_URLS: [&str; 2] = [
    "https://easylist.to/easylist/easylist.txt",
    "https://easylist.to/easylist/easyprivacy.txt",
];
/// EasyList asks clients to re-check every few days; matching that keeps the
/// cached copy fresh without downloading on every launch.
const LIST_MAX_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// The live engine. It is replaced when the downloaded lists finish parsing, so
/// every caller reads the current value instead of caching a handle.
static ENGINE: RwLock<Option<Arc<AdblockEngine>>> = RwLock::new(None);
static DEBUG_ENABLED: AtomicBool = AtomicBool::new(false);
static DEBUG_LOG_PATH: RwLock<Option<PathBuf>> = RwLock::new(None);
static DEBUG_STATS: Mutex<DebugStats> = Mutex::new(DebugStats::new());

#[derive(Default)]
struct DebugStats {
    checked: u64,
    blocked: u64,
    invalid: u64,
    blocked_hosts: Vec<String>,
}

impl DebugStats {
    const fn new() -> Self {
        Self {
            checked: 0,
            blocked: 0,
            invalid: 0,
            blocked_hosts: Vec::new(),
        }
    }
}

pub struct AdblockEngine {
    engine: Engine,
}

impl AdblockEngine {
    /// Builds an engine from raw filter list text. Empty entries are skipped so
    /// a half-finished download never replaces the fallback rules.
    #[cfg(test)]
    fn from_lists<'a>(lists: impl IntoIterator<Item = &'a str>) -> Self {
        Self::from_lists_with_debug(lists, false)
    }

    fn from_lists_with_debug<'a>(lists: impl IntoIterator<Item = &'a str>, debug: bool) -> Self {
        let mut filter_set = FilterSet::new(debug);
        for list in lists.into_iter().filter(|list| !list.trim().is_empty()) {
            filter_set.add_filter_list(list.to_owned(), ParseOptions::default());
        }
        Self {
            engine: Engine::new_with_filter_set(filter_set),
        }
    }

    /// Asks the engine whether this request should be cancelled.
    ///
    /// `source_url` must be the page the request came from: without it the
    /// engine treats every request as third-party, so `$domain=` rules and
    /// first-party exceptions never match.
    pub fn should_block(
        &self,
        url: &str,
        source_url: &str,
        request_type: &str,
        method: &str,
    ) -> bool {
        let Ok(request) = Request::new(url, source_url, request_type, method) else {
            record_invalid_request();
            return false;
        };
        let result = self.engine.check_network_request(&request);
        let blocked = result.should_block();
        record_request_decision(
            url,
            request_type,
            blocked,
            result
                .filter
                .as_ref()
                .and_then(|filter| filter.raw_line.as_deref()),
        );
        blocked
    }

    /// CSS that hides the elements the filter lists name for this exact page.
    ///
    /// Generic class/id rules are deliberately not included: they are keyed on
    /// the classes a page actually uses, and hiding every rule on every page
    /// would be both slow and destructive.
    pub fn cosmetic_css(&self, url: &str) -> String {
        let resources = self.engine.url_cosmetic_resources(url);
        let mut rules: Vec<(String, String)> = resources
            .hide_selectors
            .iter()
            .map(|selector| (selector.clone(), "display: none !important".to_owned()))
            .collect();
        let procedural_count = resources.procedural_actions.len();
        let mut converted = 0;
        for action in &resources.procedural_actions {
            if let Ok(filter) = serde_json::from_str::<ProceduralOrActionFilter>(action) {
                if let Some((selector, style)) = filter.as_css() {
                    rules.push((selector, style));
                    converted += 1;
                }
            }
        }

        rules.sort_unstable();
        rules.dedup();
        let css = rules
            .iter()
            .map(|(selector, style)| format!("{selector} {{ {style}; }}"))
            .collect::<Vec<_>>()
            .join("\n");
        if DEBUG_ENABLED.load(Ordering::Relaxed) {
            let host = url_host(url);
            append_debug_line(&format!(
                "cosmetic page={host} css_rules={} procedural={} procedural_css={} procedural_unsupported={} scriptlet_bytes={}",
                rules.len(),
                procedural_count,
                converted,
                procedural_count.saturating_sub(converted),
                resources.injected_script.len(),
            ));
        }
        css
    }

    /// Returns generic hide rules that may match the supplied DOM classes and
    /// ids, excluding rules opted out for this page's hostname.
    pub fn hidden_class_id_selectors(
        &self,
        url: &str,
        classes: &[String],
        ids: &[String],
    ) -> Vec<String> {
        let resources = self.engine.url_cosmetic_resources(url);
        if resources.generichide {
            return Vec::new();
        }

        self.engine
            .hidden_class_id_selectors(classes, ids, &resources.exceptions)
    }
}

/// Returns the current engine, or `None` before [`init`] has run.
pub(crate) fn current_engine() -> Option<Arc<AdblockEngine>> {
    ENGINE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

pub(crate) fn record_generic_cosmetic_lookup(
    page_url: &str,
    class_count: usize,
    id_count: usize,
    selector_count: usize,
) {
    append_debug_line(&format!(
        "cosmetic-generic page={} classes={} ids={} selectors={}",
        url_host(page_url),
        class_count,
        id_count,
        selector_count
    ));
}

fn install_engine(engine: AdblockEngine) {
    let mut slot = ENGINE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(Arc::new(engine));
}

/// Loads the cached lists (or the built-in fallback) and refreshes them in the
/// background when they are missing or stale.
///
/// Call this once, before the first window is created, so the very first page
/// load is already filtered.
pub fn init(app: &AppHandle) {
    let debug = init_debug_logging(app);
    let cache_dir = cache_dir(app);
    let lists = load_lists(cache_dir.as_deref());
    append_debug_line(&format!(
        "lists count={} bytes={}",
        lists.len(),
        lists.iter().map(String::len).sum::<usize>()
    ));
    install_engine(AdblockEngine::from_lists_with_debug(
        lists.iter().map(String::as_str),
        debug,
    ));

    // An explicit list directory is authoritative: never overwrite it.
    if std::env::var_os("PAKE_ADBLOCK_LISTS").is_some() {
        return;
    }
    let Some(cache_dir) = cache_dir else {
        return;
    };
    if !lists_are_stale(&cache_dir) {
        return;
    }

    tauri::async_runtime::spawn(async move {
        if let Err(error) = download_lists(&cache_dir).await {
            eprintln!("[Pake] Adblock list update failed: {error}");
        }

        let lists: Vec<String> = read_cached_lists(&cache_dir);
        if lists.is_empty() {
            return;
        }
        let engine = tauri::async_runtime::spawn_blocking(move || {
            AdblockEngine::from_lists_with_debug(lists.iter().map(String::as_str), debug)
        })
        .await;
        match engine {
            Ok(engine) => install_engine(engine),
            Err(error) => eprintln!("[Pake] Adblock engine rebuild failed: {error}"),
        }
    });
}

fn init_debug_logging(app: &AppHandle) -> bool {
    DEBUG_ENABLED.store(false, Ordering::Release);
    *DEBUG_STATS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = DebugStats::new();
    *DEBUG_LOG_PATH
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;

    let path = match app.path().app_config_dir() {
        Ok(dir) => dir.join("adblock-debug.log"),
        Err(error) => {
            eprintln!("[Pake] Cannot locate adblock diagnostic log directory: {error}");
            DEBUG_ENABLED.store(false, Ordering::Release);
            return false;
        }
    };
    let result = fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")))
        .and_then(|()| fs::write(&path, "Pake adblock diagnostics enabled\n"));
    if let Err(error) = result {
        eprintln!(
            "[Pake] Cannot initialize adblock diagnostic log {}: {error}",
            path.display()
        );
        DEBUG_ENABLED.store(false, Ordering::Release);
        return false;
    }

    *DEBUG_LOG_PATH
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path.clone());
    DEBUG_ENABLED.store(true, Ordering::Release);
    eprintln!("[Pake] Adblock diagnostics: {}", path.display());
    true
}

fn append_debug_line(line: &str) {
    if !DEBUG_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let path = DEBUG_LOG_PATH
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let Some(path) = path else {
        return;
    };
    let result = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .and_then(|mut file| writeln!(file, "{line}"));
    if let Err(error) = result {
        eprintln!(
            "[Pake] Cannot write adblock diagnostic log {}: {error}",
            path.display()
        );
    }
}

fn url_host(url: &str) -> String {
    tauri::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "invalid-url".to_owned())
}

fn record_invalid_request() {
    if DEBUG_ENABLED.load(Ordering::Relaxed) {
        DEBUG_STATS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .invalid += 1;
    }
}

fn record_request_decision(
    url: &str,
    request_type: &str,
    blocked: bool,
    matched_rule: Option<&str>,
) {
    if !DEBUG_ENABLED.load(Ordering::Relaxed) {
        return;
    }

    let host = url_host(url);
    let mut stats = DEBUG_STATS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    stats.checked += 1;
    let sample = if blocked {
        stats.blocked += 1;
        if stats.blocked_hosts.len() < 30 {
            let rule = matched_rule
                .map(|rule| {
                    rule.chars()
                        .filter(|character| !character.is_control())
                        .take(160)
                        .collect::<String>()
                })
                .unwrap_or_default();
            let sample = format!("{request_type}:{host} rule={rule}");
            stats.blocked_hosts.push(sample.clone());
            Some(sample)
        } else {
            None
        }
    } else {
        None
    };
    drop(stats);
    if let Some(sample) = sample {
        append_debug_line(&format!("blocked {sample}"));
    }
}

/// Writes per-page request totals without recording paths, query strings, or fragments.
pub fn flush_debug_stats(page_url: &str) {
    if !DEBUG_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let stats = {
        let mut stats = DEBUG_STATS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::replace(&mut *stats, DebugStats::new())
    };
    append_debug_line(&format!(
        "page={} checked={} blocked={} invalid={} blocked_hosts={}",
        url_host(page_url),
        stats.checked,
        stats.blocked,
        stats.invalid,
        stats.blocked_hosts.join(",")
    ));
}

/// `PAKE_ADBLOCK_LISTS` wins over the app config directory.
fn cache_dir(app: &AppHandle) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("PAKE_ADBLOCK_LISTS") {
        return Some(PathBuf::from(dir));
    }
    app.path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join(CACHE_SUBDIR))
}

fn read_cached_lists(dir: &Path) -> Vec<String> {
    LIST_FILES
        .iter()
        .filter_map(|name| fs::read_to_string(dir.join(name)).ok())
        .collect()
}

/// Cached lists when present, otherwise the built-in fallback rules.
fn load_lists(dir: Option<&Path>) -> Vec<String> {
    let cached = dir.map(read_cached_lists).unwrap_or_default();
    if cached.iter().any(|list| !list.trim().is_empty()) {
        cached
    } else {
        vec![DEFAULT_ADBLOCK_RULES.to_owned()]
    }
}

fn lists_are_stale(dir: &Path) -> bool {
    LIST_FILES.iter().any(|name| {
        fs::metadata(dir.join(name))
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .map(|age| age > LIST_MAX_AGE)
            .unwrap_or(true)
    })
}

async fn download_lists(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|error| format!("cannot create {}: {error}", dir.display()))?;

    let client = reqwest::Client::builder()
        .user_agent(concat!("Pake/", env!("CARGO_PKG_VERSION")))
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?;

    for (name, url) in LIST_FILES.iter().zip(LIST_URLS) {
        let response = client
            .get(url)
            .send()
            .await
            .map_err(|error| format!("{url}: {error}"))?;
        if !response.status().is_success() {
            return Err(format!("{url} returned HTTP {}", response.status()));
        }
        let text = response
            .text()
            .await
            .map_err(|error| format!("{url}: {error}"))?;
        if text.trim().is_empty() {
            return Err(format!("{url} returned an empty list"));
        }

        // Write then rename so a crash mid-write never leaves a truncated list
        // that the next launch would happily parse.
        let path = dir.join(name);
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, text.as_bytes())
            .map_err(|error| format!("cannot write {}: {error}", temporary.display()))?;
        fs::rename(&temporary, &path)
            .map_err(|error| format!("cannot replace {}: {error}", path.display()))?;
    }

    Ok(())
}

/// Runs the engine for one network request. Safe to call from the UI thread.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn should_block(url: &str, source_url: &str, request_type: &str, method: &str) -> bool {
    current_engine()
        .map(|engine| engine.should_block(url, source_url, request_type, method))
        .unwrap_or(false)
}

fn js_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

/// Init script that installs the curated generic sheet before first paint.
pub fn get_init_cosmetic_script() -> String {
    format!(
        "if (window.__PAKE_INJECT_STYLE__) {{ window.__PAKE_INJECT_STYLE__({}, 'pake-adblock-generic'); }}",
        js_string(GENERIC_COSMETIC_CSS)
    )
}

/// Page-load script that hides the selectors the filter lists name for `url`.
pub fn cosmetic_page_script(url: &str) -> Option<String> {
    let css = current_engine()?.cosmetic_css(url);
    if css.is_empty() {
        return None;
    }
    Some(format!(
        "if (window.__PAKE_INJECT_STYLE__) {{ window.__PAKE_INJECT_STYLE__({}, 'pake-adblock-cosmetic'); }}",
        js_string(&css)
    ))
}

/// Installs the WebView2 request handler. Windows only for now; the call site is
/// already gated, this keeps the module usable from every platform.
#[cfg(target_os = "windows")]
pub fn install(window: &WebviewWindow) -> tauri::Result<()> {
    let owner = window.clone();
    window.with_webview(move |webview| {
        let result = unsafe {
            (|| -> windows_core::Result<()> {
                let core = webview.controller().CoreWebView2()?;
                // Must be registered before the first navigation. Every
                // subresource context is filtered; documents are skipped below.
                core.AddWebResourceRequestedFilter(
                    windows_core::w!("*"),
                    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
                )?;
                let window = owner.clone();
                let mut token = 0;
                core.add_WebResourceRequested(
                    &WebResourceRequestedEventHandler::create(Box::new(move |sender, args| {
                        let (Some(sender), Some(args)) = (sender, args) else {
                            return Ok(());
                        };
                        if let Err(error) = handle_request(&sender, &args, &window) {
                            eprintln!("[Pake] Adblock request handling failed: {error}");
                        }
                        Ok(())
                    })),
                    &mut token,
                )?;
                append_debug_line("WebView2 network request handler registered");
                match core.cast::<ICoreWebView2_4>() {
                    Ok(core4) => {
                        let frame_window = owner.clone();
                        let mut frame_token = 0;
                        if let Err(error) = core4.add_FrameCreated(
                            &FrameCreatedEventHandler::create(Box::new(move |_, args| {
                                let Some(args) = args else {
                                    return Ok(());
                                };
                                let frame = args.Frame()?.cast::<ICoreWebView2Frame2>()?;
                                let window = frame_window.clone();
                                let mut token = 0;
                                if let Err(error) = frame.add_NavigationStarting(
                                    &FrameNavigationStartingEventHandler::create(Box::new(
                                        move |_, args| {
                                            if let Some(args) = args {
                                                if let Err(error) =
                                                    handle_frame_navigation(&args, &window)
                                                {
                                                    eprintln!(
                                                        "[Pake] Adblock frame navigation failed: {error}"
                                                    );
                                                }
                                            }
                                            Ok(())
                                        },
                                    )),
                                    &mut token,
                                ) {
                                    eprintln!(
                                        "[Pake] Failed to install adblock frame handler: {error}"
                                    );
                                }
                                Ok(())
                            })),
                            &mut frame_token,
                        ) {
                            append_debug_line("WebView2 frame listener registration failed");
                            eprintln!("[Pake] Failed to listen for WebView2 frames: {error}");
                        } else {
                            append_debug_line("WebView2 frame listener registered");
                        }
                    }
                    Err(error) => {
                        append_debug_line("WebView2 frame filtering interface unavailable");
                        eprintln!("[Pake] WebView2 frame filtering is unavailable: {error}");
                    }
                }
                Ok(())
            })()
        };
        if let Err(error) = result {
            eprintln!("[Pake] Failed to initialize Windows adblock: {error}");
        }
    })
}

#[cfg(target_os = "windows")]
unsafe fn handle_frame_navigation(
    args: &ICoreWebView2NavigationStartingEventArgs,
    window: &WebviewWindow,
) -> windows_core::Result<()> {
    let mut uri = PWSTR::null();
    args.Uri(&mut uri)?;
    let uri = take_pwstr(uri);
    if !(uri.starts_with("http://") || uri.starts_with("https://")) {
        return Ok(());
    }

    let source_url = window.url().map(|url| url.to_string()).unwrap_or_default();
    if should_block(&uri, &source_url, "subdocument", "GET") {
        args.SetCancel(true)?;
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn install(_window: &tauri::WebviewWindow) -> tauri::Result<()> {
    Ok(())
}

#[cfg(target_os = "windows")]
unsafe fn handle_request(
    sender: &ICoreWebView2,
    args: &ICoreWebView2WebResourceRequestedEventArgs,
    window: &WebviewWindow,
) -> windows_core::Result<()> {
    let mut context = COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL;
    args.ResourceContext(&mut context)?;
    // Blocking a top-level document would replace the app's page with an error
    // screen, and ad filter lists do not target first-party documents anyway.
    if context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT {
        return Ok(());
    }

    let request = args.Request()?;
    let mut uri = PWSTR::null();
    request.Uri(&mut uri)?;
    let uri = take_pwstr(uri);
    if !(uri.starts_with("http://") || uri.starts_with("https://")) {
        return Ok(());
    }

    let mut method = PWSTR::null();
    request.Method(&mut method)?;
    let method = take_pwstr(method);

    let source_url = referer(&request)
        .unwrap_or_else(|| window.url().map(|url| url.to_string()).unwrap_or_default());

    if !should_block(&uri, &source_url, resource_type(context), &method) {
        return Ok(());
    }

    let core2 = sender.cast::<ICoreWebView2_2>()?;
    let environment = core2.Environment()?;
    let response = environment.CreateWebResourceResponse(
        None,
        403,
        windows_core::w!("Forbidden"),
        windows_core::w!(""),
    )?;
    args.SetResponse(&response)
}

#[cfg(target_os = "windows")]
unsafe fn referer(request: &ICoreWebView2WebResourceRequest) -> Option<String> {
    let headers = request.Headers().ok()?;
    let mut referer = PWSTR::null();
    headers
        .GetHeader(windows_core::w!("Referer"), &mut referer)
        .ok()?;
    if referer.is_null() {
        return None;
    }
    let referer = take_pwstr(referer);
    (!referer.is_empty()).then_some(referer)
}

/// Maps a WebView2 resource context onto the names adblock-rust understands.
#[cfg(target_os = "windows")]
fn resource_type(context: COREWEBVIEW2_WEB_RESOURCE_CONTEXT) -> &'static str {
    match context {
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT => "document",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET => "stylesheet",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE => "image",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA => "media",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT => "font",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT => "script",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST => "xmlhttprequest",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH => "fetch",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING => "ping",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET => "websocket",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MANIFEST | COREWEBVIEW2_WEB_RESOURCE_CONTEXT_OTHER => {
            "other"
        }
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> AdblockEngine {
        AdblockEngine::from_lists([DEFAULT_ADBLOCK_RULES])
    }

    #[test]
    fn built_in_rules_block_ad_and_tracker_hosts() {
        let engine = engine();
        for url in [
            "https://pagead2.googlesyndication.com/pagead/js/adsbygoogle.js",
            "https://adservice.google.com/adsid/integrator.js",
            "https://www.google-analytics.com/analytics.js",
            "https://static.criteo.net/js/ld/publishertag.js",
        ] {
            assert!(
                engine.should_block(url, "https://example.com", "script", "GET"),
                "{url} should be blocked"
            );
        }
    }

    #[test]
    fn built_in_rules_leave_ordinary_resources_alone() {
        let engine = engine();
        for url in [
            "https://example.com/main.js",
            "https://fonts.googleapis.com/css",
            "https://cdn.jsdelivr.net/npm/vue/dist/vue.js",
        ] {
            assert!(
                !engine.should_block(url, "https://example.com", "script", "GET"),
                "{url} should not be blocked"
            );
        }
    }

    #[test]
    fn subdocument_rules_block_ad_iframes_without_blocking_main_documents() {
        let engine = AdblockEngine::from_lists(["||ads.example^$subdocument"]);

        assert!(engine.should_block(
            "https://ads.example/banner",
            "https://site.example/",
            "subdocument",
            "GET"
        ));
        assert!(!engine.should_block(
            "https://ads.example/banner",
            "https://site.example/",
            "document",
            "GET"
        ));
    }

    #[test]
    fn a_missing_source_url_still_blocks_third_party_requests() {
        let engine = engine();
        assert!(engine.should_block("https://doubleclick.net/ad.js", "", "script", "GET"));
    }

    #[test]
    fn cosmetic_css_uses_the_hostname_rules_for_that_page() {
        let engine =
            AdblockEngine::from_lists(["example.com##.ad-slot\nexample.com##.promo-banner"]);
        let css = engine.cosmetic_css("https://example.com/article");
        assert!(css.contains(".ad-slot"), "{css}");
        assert!(css.contains(".promo-banner"), "{css}");
        assert!(css.contains("display: none !important"));
    }

    #[test]
    fn cosmetic_css_includes_css_compatible_action_filters() {
        let engine = AdblockEngine::from_lists(["example.com##.ad-card:style(color: red)"]);
        let css = engine.cosmetic_css("https://example.com/article");
        assert!(css.contains(".ad-card"), "{css}");
        assert!(css.contains("color: red"), "{css}");
    }

    #[test]
    fn cosmetic_css_is_empty_for_unrelated_pages() {
        let engine = AdblockEngine::from_lists(["example.com##.ad-slot"]);
        assert!(engine.cosmetic_css("https://other.test/").is_empty());
    }

    #[test]
    fn generic_cosmetic_rules_match_dom_classes_and_ids() {
        let engine = AdblockEngine::from_lists(["##.ad-slot\n###top-banner"]);
        let selectors = engine.hidden_class_id_selectors(
            "https://example.com/article",
            &["ad-slot".to_owned(), "article".to_owned()],
            &["top-banner".to_owned()],
        );

        assert!(selectors.contains(&".ad-slot".to_owned()));
        assert!(selectors.contains(&"#top-banner".to_owned()));
        assert!(!selectors.contains(&".article".to_owned()));
    }

    #[test]
    fn generic_cosmetic_rules_respect_hostname_exceptions() {
        let engine = AdblockEngine::from_lists(["##.ad-slot\nexample.com#@#.ad-slot"]);
        let selectors = engine.hidden_class_id_selectors(
            "https://example.com/article",
            &["ad-slot".to_owned()],
            &[],
        );
        assert!(selectors.is_empty());
    }

    #[test]
    fn diagnostics_only_keep_the_hostname_from_a_url() {
        assert_eq!(
            url_host("https://ads.example/path?user=private#section"),
            "ads.example"
        );
    }

    #[test]
    fn generic_sheet_names_the_common_ad_containers() {
        for selector in [".adsbygoogle", "[id^=\"div-gpt-ad\"]", ".sponsored"] {
            assert!(
                GENERIC_COSMETIC_CSS.contains(selector),
                "generic sheet is missing {selector}"
            );
        }
    }

    #[test]
    fn injected_scripts_are_valid_javascript_strings() {
        let script = get_init_cosmetic_script();
        assert!(script.starts_with("if (window.__PAKE_INJECT_STYLE__)"));
        // serde_json escaping keeps the CSS on a single line.
        assert!(!script.contains('\n'));
    }

    #[test]
    fn empty_lists_never_replace_the_engine_with_nothing() {
        let engine = AdblockEngine::from_lists(["", "   "]);
        assert!(!engine.should_block("https://example.com/a.js", "", "script", "GET"));
    }

    /// Maintenance check, not part of the default suite: point
    /// `PAKE_ADBLOCK_LISTS` at a directory holding `easylist.txt` and
    /// `easyprivacy.txt` to prove the published lists still parse and block.
    #[test]
    #[ignore = "needs PAKE_ADBLOCK_LISTS pointing at downloaded filter lists"]
    fn real_filter_lists_parse_and_block() {
        let dir = PathBuf::from(
            std::env::var("PAKE_ADBLOCK_LISTS").expect("PAKE_ADBLOCK_LISTS must be set"),
        );
        let lists = read_cached_lists(&dir);
        assert_eq!(
            lists.len(),
            LIST_FILES.len(),
            "both filter lists should be readable"
        );

        let engine = AdblockEngine::from_lists(lists.iter().map(String::as_str));
        for url in [
            "https://pagead2.googlesyndication.com/pagead/js/adsbygoogle.js",
            "https://www.google-analytics.com/analytics.js",
            "https://securepubads.g.doubleclick.net/tag/js/gpt.js",
        ] {
            assert!(
                engine.should_block(url, "https://example.com", "script", "GET"),
                "{url} should be blocked by the downloaded lists"
            );
        }
        assert!(!engine.should_block(
            "https://example.com/app.js",
            "https://example.com",
            "script",
            "GET"
        ));
    }
}
