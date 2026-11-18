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

struct Page {
    view: WebView,
    _context: WebContext,
    loaded: watch::Receiver<bool>,
    waiting: Waiting,
    next: Cell<u64>,
}

impl Page {
    fn open(window: &dioxus::desktop::tao::window::Window) -> Result<Self, String> {
        let data = directories::ProjectDirs::from("moe", "kopuz", "kopuz")
            .map(|dirs| dirs.cache_dir().join("soundcloud-page"))
            .unwrap_or_else(|| std::path::PathBuf::from("./cache/soundcloud-page"));
        let _ = std::fs::create_dir_all(&data);
        let mut context = WebContext::new(Some(data));

        let waiting: Waiting = Rc::default();
        let answers = waiting.clone();
        let (loaded_tx, loaded) = watch::channel(false);

        let view = WebViewBuilder::new_with_web_context(&mut context)
            .with_url(HOME)
            .with_visible(false)
            .with_bounds(Rect {
                position: LogicalPosition::new(0, 0).into(),
                size: LogicalSize::new(1, 1).into(),
            })
            .with_on_page_load_handler(move |event, _url| {
                if matches!(event, PageLoadEvent::Finished) {
                    let _ = loaded_tx.send(true);
                }
            })
            .with_ipc_handler(move |message| answer(&answers, message.body()))
            .build_as_child(window)
            .map_err(|error| format!("SoundCloud page: {error}"))?;
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
  try {{
    const response = await fetch({url}, {{ method: {method}, credentials: "include", headers: {headers} }});
    window.ipc.postMessage(JSON.stringify({{ id, status: response.status }}));
  }} catch (error) {{
    window.ipc.postMessage(JSON.stringify({{ id, status: 0, error: String(error) }}));
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
fn answer(waiting: &Waiting, body: &str) {
    let Ok(reply) = serde_json::from_str::<serde_json::Value>(body) else {
        return;
    };
    let Some(id) = reply["id"].as_u64() else {
        return;
    };
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
