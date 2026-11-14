//! Drive the running browser over its own CDP port: the GUI's address field opens a URL in the
//! window that is already up (`inocode-browser --url=..` when one is running).

use serde_json::{json, Value};

/// The page the window shows: the first `page` target that is not DevTools or a Chrome UI page.
fn page_socket(port: u16) -> Result<String, String> {
    let text = ureq::get(&format!("http://127.0.0.1:{port}/json"))
        .call()
        .map_err(|e| e.to_string())?
        .into_body()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let list: Vec<Value> = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    list.iter()
        .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        .filter(|t| {
            let url = t.get("url").and_then(Value::as_str).unwrap_or("");
            !url.starts_with("devtools://") && !url.starts_with("chrome://")
        })
        .find_map(|t| t.get("webSocketDebuggerUrl").and_then(Value::as_str).map(str::to_string))
        .ok_or_else(|| "no page to open it in".to_string())
}

/// `Page.navigate` in the shown page.
pub fn navigate(port: u16, url: &str) -> Result<(), String> {
    let socket = page_socket(port)?;
    let (mut ws, _) = tungstenite::connect(socket.as_str()).map_err(|e| e.to_string())?;
    let request = json!({ "id": 1, "method": "Page.navigate", "params": { "url": url } });
    ws.send(tungstenite::Message::text(request.to_string())).map_err(|e| e.to_string())?;
    loop {
        let msg = ws.read().map_err(|e| e.to_string())?;
        let Ok(text) = msg.to_text() else { continue };
        let Ok(v) = serde_json::from_str::<Value>(text) else { continue };
        if v.get("id").and_then(Value::as_i64) != Some(1) {
            continue;
        }
        let _ = ws.close(None);
        if let Some(err) = v.get("error") {
            return Err(err.get("message").and_then(Value::as_str).unwrap_or("navigate failed").to_string());
        }
        if let Some(text) = v.pointer("/result/errorText").and_then(Value::as_str) {
            return Err(text.to_string());
        }
        return Ok(());
    }
}
