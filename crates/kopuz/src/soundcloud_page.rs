//! A hidden soundcloud.com page that sends the account's SoundCloud likes.
//!
//! SoundCloud refuses likes that do not come from a browser, so the like is
//! sent from a real soundcloud.com page inside the app, the same request the
//! web player makes. The page is a hidden child of the main window, opened on
//! the first like and kept for the rest of the session. It has its own
//! storage, separate from the app's web view.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use dioxus::desktop::tao::window::Window;
use dioxus::desktop::wry::dpi::{LogicalPosition, LogicalSize};
use dioxus::desktop::wry::{PageLoadEvent, Rect, WebContext, WebView, WebViewBuilder};
use dioxus::prelude::*;
use server::soundcloud::page::Request;
use tokio::sync::{oneshot, watch};

const HOME: &str = "https://soundcloud.com/";

type Waiting = Rc<RefCell<HashMap<u64, oneshot::Sender<Result<u16, String>>>>>;

/// Serve the core's SoundCloud page requests for as long as the app runs.
pub fn use_soundcloud_page() {
    let window = dioxus::desktop::use_window();
    use_hook(move || {
        let Some(mut requests) = server::soundcloud::page::serve() else {
            return;
        };
        spawn(async move {
            let mut page: Option<Page> = None;
            while let Some(request) = requests.recv().await {
                if page.is_none() {
                    match Page::open(&window.window) {
                        Ok(opened) => page = Some(opened),
                        Err(error) => {
                            tracing::warn!(%error, "couldn't open the SoundCloud page");
                            let _ = request.reply.send(Err(error));
                            continue;
                        }
                    }
                }
                let Some(open) = page.as_ref() else { continue };
                open.ready().await;
                open.send(request);
            }
        });
    });
}

/// The page and the window it sits in, once it is built.
#[derive(Clone, Default)]
struct Shown(Rc<RefCell<Option<(Rc<WebView>, Arc<Window>)>>>);

impl Shown {
    /// Show the page over the middle of the window, or hide it again.
    fn set(&self, visible: bool) {
        let Some((view, window)) = self.0.borrow().clone() else {
            return;
        };
        let bounds = if visible {
            let size = window.inner_size().to_logical::<f64>(window.scale_factor());
            let width = size.width.min(460.0);
            let height = size.height.min(640.0);
            Rect {
                position: LogicalPosition::new(
                    (size.width - width) / 2.0,
                    (size.height - height) / 2.0,
                )
                .into(),
                size: LogicalSize::new(width, height).into(),
            }
        } else {
            hidden_bounds()
        };
        let _ = view.set_bounds(bounds);
        let _ = view.set_visible(visible);
        if visible {
            let _ = view.focus();
        }
    }
}

fn hidden_bounds() -> Rect {
    Rect {
        position: LogicalPosition::new(0, 0).into(),
        size: LogicalSize::new(1, 1).into(),
    }
}

struct Page {
    view: Rc<WebView>,
    _context: WebContext,
    loaded: watch::Receiver<bool>,
    waiting: Waiting,
    next: Cell<u64>,
}

impl Page {
    fn open(window: &Arc<Window>) -> Result<Self, String> {
        let data = directories::ProjectDirs::from("moe", "kopuz", "kopuz")
            .map(|dirs| dirs.cache_dir().join("soundcloud-page"))
            .unwrap_or_else(|| std::path::PathBuf::from("./cache/soundcloud-page"));
        let _ = std::fs::create_dir_all(&data);
        let mut context = WebContext::new(Some(data));

        let waiting: Waiting = Rc::default();
        let answers = waiting.clone();
        let shown = Shown::default();
        let show = shown.clone();
        let (loaded_tx, loaded) = watch::channel(false);

        let view = WebViewBuilder::new_with_web_context(&mut context)
            .with_url(HOME)
            .with_visible(false)
            .with_bounds(hidden_bounds())
            .with_on_page_load_handler(move |event, _url| {
                if matches!(event, PageLoadEvent::Finished) {
                    let _ = loaded_tx.send(true);
                }
            })
            .with_ipc_handler(move |message| answer(&answers, &show, message.body()))
            .build_as_child(&**window)
            .map_err(|error| format!("SoundCloud page: {error}"))?;
        let view = Rc::new(view);
        *shown.0.borrow_mut() = Some((view.clone(), window.clone()));
        tracing::info!("opened the SoundCloud page for likes");

        Ok(Self {
            view,
            _context: context,
            loaded,
            waiting,
            next: Cell::new(0),
        })
    }

    async fn ready(&self) {
        let mut loaded = self.loaded.clone();
        let _ = loaded.wait_for(|loaded| *loaded).await;
    }

    fn send(&self, request: Request) {
        let Request {
            method,
            url,
            headers,
            reply,
        } = request;
        let id = self.next.get();
        self.next.set(id + 1);
        self.waiting.borrow_mut().insert(id, reply);

        let headers: serde_json::Map<String, serde_json::Value> = headers
            .into_iter()
            .map(|(name, value)| (name, serde_json::Value::String(value)))
            .collect();
        let script = format!(
            r#"(async () => {{
  const id = {id};
  const url = {url};
  const method = {method};
  const base = {headers};
  const post = (message) => window.ipc.postMessage(JSON.stringify(Object.assign({{ id }}, message)));
  // The web player sends its DataDome cookie as a header too.
  const send = () => {{
    const headers = Object.assign({{}}, base);
    const datadome = document.cookie.match(/(?:^|;\s*)datadome=([^;]+)/);
    if (datadome) headers["x-datadome-clientid"] = datadome[1];
    return fetch(url, {{ method, credentials: "include", headers }});
  }};
  // SoundCloud asked for a check: show it, and wait for the person to pass it.
  const check = (target) => new Promise((resolve) => {{
    const frame = document.createElement("iframe");
    frame.src = target;
    frame.style.cssText = "position:fixed;inset:0;width:100%;height:100%;border:0;z-index:2147483647;background:#fff";
    const passed = (event) => {{
      let host = "";
      try {{ host = new URL(event.origin).hostname; }} catch (_) {{ return; }}
      if (!host.endsWith("captcha-delivery.com")) return;
      let data = event.data;
      try {{ if (typeof data === "string") data = JSON.parse(data); }} catch (_) {{ return; }}
      if (!data || typeof data.cookie !== "string") return;
      document.cookie = data.cookie;
      window.removeEventListener("message", passed);
      frame.remove();
      resolve();
    }};
    window.addEventListener("message", passed);
    document.body.appendChild(frame);
  }});
  try {{
    let response = await send();
    if (response.status === 403) {{
      let body = null;
      try {{ body = await response.clone().json(); }} catch (_) {{}}
      if (body && typeof body.url === "string" && body.url.includes("captcha-delivery.com")) {{
        post({{ check: true }});
        await check(body.url);
        post({{ check: false }});
        response = await send();
      }}
    }}
    post({{ status: response.status }});
  }} catch (error) {{
    post({{ status: 0, error: String(error) }});
  }}
}})();"#,
            url = serde_json::Value::String(url),
            method = serde_json::Value::String(method),
            headers = serde_json::Value::Object(headers),
        );
        if let Err(error) = self.view.evaluate_script(&script)
            && let Some(reply) = self.waiting.borrow_mut().remove(&id)
        {
            let _ = reply.send(Err(format!("SoundCloud page: {error}")));
        }
    }
}

/// Hand a reply from the page to the request waiting for it.
fn answer(waiting: &Waiting, shown: &Shown, body: &str) {
    let Ok(reply) = serde_json::from_str::<serde_json::Value>(body) else {
        return;
    };
    let Some(id) = reply["id"].as_u64() else {
        return;
    };
    // SoundCloud wants a check the person has to pass themselves.
    if let Some(check) = reply["check"].as_bool() {
        tracing::info!(check, "SoundCloud asked for a check before the like");
        shown.set(check);
        return;
    }
    shown.set(false);
    let Some(sender) = waiting.borrow_mut().remove(&id) else {
        return;
    };
    let status = reply["status"].as_u64().unwrap_or(0) as u16;
    let result = match reply["error"].as_str() {
        Some(error) if status == 0 => Err(format!("SoundCloud page: {error}")),
        _ => Ok(status),
    };
    let _ = sender.send(result);
}
