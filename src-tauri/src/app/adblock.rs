//! Adblock integration using Brave's adblock-rust engine.

use adblock::engine::Engine;
use adblock::request::Request;
use std::sync::{Arc, OnceLock};

#[cfg(target_os = "windows")]
use tauri::WebviewWindow;
#[cfg(target_os = "windows")]
use webview2_com::{
    take_pwstr,
    Microsoft::Web::WebView2::Win32::{
        ICoreWebView2_2, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET,
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST,
    },
    WebResourceRequestedEventHandler,
};
#[cfg(target_os = "windows")]
use windows_core::{Interface, PWSTR};

pub static ADBLOCK_ENGINE: OnceLock<Arc<AdblockEngine>> = OnceLock::new();

pub struct AdblockEngine {
    engine: Engine,
}

impl AdblockEngine {
    pub fn new(rules: &str) -> Self {
        let engine = Engine::new_with_list_text(rules);
        Self { engine }
    }

    pub fn should_block(
        &self,
        url: &str,
        source_url: &str,
        request_type: &str,
        method: &str,
    ) -> bool {
        match Request::new(url, source_url, request_type, method) {
            Ok(request) => {
                let blocker_result = self.engine.check_network_request(&request);
                blocker_result.should_block()
            }
            Err(_) => false,
        }
    }

    pub fn get_cosmetic_css(&self, url: &str) -> Option<String> {
        let resources = self.engine.url_cosmetic_resources(url);
        if resources.hide_selectors.is_empty() {
            None
        } else {
            let mut selectors: Vec<&str> = resources
                .hide_selectors
                .iter()
                .map(String::as_str)
                .collect();
            selectors.sort_unstable();
            let joined = selectors.join(", ");
            Some(format!("{joined} {{ display: none !important; }}"))
        }
    }

    pub fn get_generic_cosmetic_css(&self) -> &'static str {
        GENERIC_COSMETIC_CSS
    }
}

pub fn get_adblock_engine() -> &'static Arc<AdblockEngine> {
    ADBLOCK_ENGINE.get_or_init(|| Arc::new(AdblockEngine::new(DEFAULT_ADBLOCK_RULES)))
}

pub fn get_init_cosmetic_script() -> String {
    let css = get_adblock_engine().get_generic_cosmetic_css();
    format!(
        "if (window.__PAKE_INJECT_STYLE__) {{ window.__PAKE_INJECT_STYLE__({:?}, 'pake-adblock-generic'); }}",
        css
    )
}

#[cfg(target_os = "windows")]
pub fn install(window: &WebviewWindow) -> tauri::Result<()> {
    window.with_webview(move |webview| {
        let result = unsafe {
            (|| -> windows_core::Result<()> {
                let core = webview.controller().CoreWebView2()?;
                core.AddWebResourceRequestedFilter(
                    windows_core::w!("*"),
                    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
                )?;
                let engine = get_adblock_engine().clone();
                let mut token = 0;
                core.add_WebResourceRequested(
                    &WebResourceRequestedEventHandler::create(Box::new(move |sender, args| {
                        let Some(args) = args else { return Ok(()) };
                        let Some(sender) = sender else { return Ok(()) };
                        let request = args.Request()?;
                        let mut uri = PWSTR::null();
                        request.Uri(&mut uri)?;
                        let uri_str = take_pwstr(uri);

                        let mut method = PWSTR::null();
                        request.Method(&mut method)?;
                        let method_str = take_pwstr(method);

                        let mut context = COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL;
                        args.ResourceContext(&mut context)?;
                        let req_type = match context {
                            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE => "image",
                            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET => "stylesheet",
                            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT => "script",
                            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT => "font",
                            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA => "media",
                            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST => "xmlhttprequest",
                            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH => "fetch",
                            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING => "ping",
                            _ => "other",
                        };

                        if engine.should_block(&uri_str, "", req_type, &method_str) {
                            let core2 = sender.cast::<ICoreWebView2_2>()?;
                            let env = core2.Environment()?;
                            let response = env.CreateWebResourceResponse(
                                None,
                                403,
                                windows_core::w!("Blocked by Pake adblock"),
                                windows_core::w!(""),
                            )?;
                            args.SetResponse(&response)?;
                        }
                        Ok(())
                    })),
                    &mut token,
                )?;
                Ok(())
            })()
        };
        if let Err(error) = result {
            eprintln!("[Pake] Failed to initialize Windows adblock: {error}");
        }
    })
}

#[cfg(not(target_os = "windows"))]
pub fn install(_window: &tauri::WebviewWindow) -> tauri::Result<()> {
    Ok(())
}

pub const GENERIC_COSMETIC_CSS: &str = r#"
.adsbygoogle, [id^="google_ads_"], [id^="div-gpt-ad"], .ad-banner, .ad-unit, .ad-zone, .ad-box, .ad-container, .ad-placeholder, .ad_wrapper, .advertisement, .sponsored-post, .native-ad, [data-ad-client], [data-ad-slot], [data-adunit], [class*="google-ads"], [class*="ad-placement"], [class*="dfp-ad"], [class*="banner-ad"], [id*="banner-ad"] { display: none !important; }
"#;

pub const DEFAULT_ADBLOCK_RULES: &str = r#"
! High-impact common ad and tracking filters
||doubleclick.net^
||googlesyndication.com^
||google-analytics.com^
||googleadservices.com^
||pagead2.googlesyndication.com^
||adservice.google.com^
||adnxs.com^
||ads.pubmatic.com^
||amazon-adsystem.com^
||criteo.com^
||criteo.net^
||scorecardresearch.com^
||taboola.com^
||outbrain.com^
||pixel.facebook.com^
||analytics.tiktok.com^
||hotjar.com^
||clarity.ms^
||quantserve.com^
||rubiconproject.com^
||casalemedia.com^
||openx.net^
||advertising.com^
||bidswitch.net^
||smartadserver.com^
||popads.net^
||popcash.net^
||adroll.com^
||moatads.com^
||adsystem.com^
||adsafeprotected.com^
||adzerk.net^
||exponential.com^
||yieldmo.com^
||zergnet.com^
||media.net^
||adcolony.com^
||unityads.unity3d.com^
||vungle.com^
||inmobi.com^
||chartbeat.com^
||statcounter.com^
||histats.com^
||mc.yandex.ru^

! Generic element hiding rules
##.adsbygoogle
##[id^="google_ads_"]
##[id^="div-gpt-ad"]
##.ad-banner
##.ad-unit
##.ad-zone
##.ad-box
##.ad-container
##.ad-placeholder
##.ad_wrapper
##.advertisement
##.sponsored-post
##.native-ad
##[data-ad-client]
##[data-ad-slot]
##[data-adunit]
##[class*="google-ads"]
##[class*="ad-placement"]
##[class*="dfp-ad"]
##[class*="banner-ad"]
##[id*="banner-ad"]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adblock_network_filter() {
        let engine = AdblockEngine::new(DEFAULT_ADBLOCK_RULES);
        assert!(engine.should_block(
            "https://pagead2.googlesyndication.com/pagead/js/adsbygoogle.js",
            "https://example.com",
            "script",
            "GET"
        ));
        assert!(engine.should_block(
            "https://adservice.google.com/adsid/integrator.js",
            "https://example.com",
            "script",
            "GET"
        ));
        assert!(engine.should_block(
            "https://google-analytics.com/analytics.js",
            "https://example.com",
            "script",
            "GET"
        ));
        assert!(!engine.should_block(
            "https://example.com/main.js",
            "https://example.com",
            "script",
            "GET"
        ));
        assert!(!engine.should_block(
            "https://fonts.googleapis.com/css",
            "https://example.com",
            "stylesheet",
            "GET"
        ));
    }

    #[test]
    fn test_adblock_cosmetic_filter() {
        let engine = AdblockEngine::new("example.com##.adsbygoogle\n||doubleclick.net^");
        let css = engine.get_cosmetic_css("https://example.com");
        assert!(css.is_some());
        let css_str = css.unwrap();
        assert!(css_str.contains(".adsbygoogle"));
        assert!(css_str.contains("display: none !important;"));
    }

    #[test]
    fn test_generic_cosmetic_css() {
        let engine = AdblockEngine::new(DEFAULT_ADBLOCK_RULES);
        let generic = engine.get_generic_cosmetic_css();
        assert!(generic.contains(".adsbygoogle"));
        assert!(generic.contains(".ad-banner"));
    }
}
