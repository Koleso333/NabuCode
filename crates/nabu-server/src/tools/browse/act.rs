use std::path::Path;

use serde_json::{Value, json};

use super::tab::Tab;
use crate::tools::resolve;

/// What an action points at: an element by its number, or a point in screenshot pixels.
enum Target {
    Element(u32, i64),
    Point(f64, f64),
}

pub async fn run(tab: &Tab, action: &Value, directory: &Path) -> Result<(), String> {
    // The page replaced the element since the last state: numbers move to the elements now there, and the action runs once more.
    let result = match run_action(tab, action, directory).await {
        Err(e) if gone(&e) => {
            tab.renumber().await?;
            run_action(tab, action, directory).await
        }
        other => other,
    };
    result.map_err(|e| match e.contains("does not belong to the document") || e.contains("No node with given id") {
        true => "the element is no longer on the page; the state below shows the page now".into(),
        false => e,
    })
}

/// Errors of an element number that points at nothing on the page now.
fn gone(error: &str) -> bool {
    ["there is no element [", "was on the page before", "is no longer on the page", "does not belong to the document", "No node with given id", "detached"]
        .iter()
        .any(|text| error.contains(text))
}

async fn run_action(tab: &Tab, action: &Value, directory: &Path) -> Result<(), String> {
    let kind = action["do"].as_str().ok_or("an action needs `do`")?;
    match kind {
        "click" => click(tab, action).await,
        "hover" => {
            let (x, y) = point(tab, &target(tab, &action["target"])?).await?;
            mouse(tab, "mouseMoved", x, y, "none", 0, 0).await
        }
        "type" => type_text(tab, action).await,
        "fill" => fill(tab, action).await,
        "key" => press(tab, action["keys"].as_str().ok_or("`key` needs `keys`, like \"Enter\" or \"ctrl+a\"")?).await,
        "scroll" => scroll(tab, action).await,
        "drag" => drag(tab, action).await,
        "upload" => upload(tab, action, directory).await,
        "dialog" => dialog(tab, action).await,
        "goto" => goto(tab, action["url"].as_str().ok_or("`goto` needs `url`")?).await,
        "back" => history(tab, -1).await,
        "forward" => history(tab, 1).await,
        "reload" => tab.call("Page.reload", json!({})).await.map(drop),
        "viewport" => viewport(tab, action).await,
        other => {
            return Err(format!(
                "unknown action `{other}`; actions: click, hover, type, fill, key, scroll, drag, upload, dialog, goto, back, forward, reload, viewport"
            ));
        }
    }
}

fn target(tab: &Tab, value: &Value) -> Result<Target, String> {
    if let Some([x, y]) = value.as_array().map(Vec::as_slice)
        && let (Some(x), Some(y)) = (x.as_f64(), y.as_f64())
    {
        return Ok(Target::Point(x, y));
    }
    let number = match value {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().trim_start_matches('[').trim_end_matches(']').parse().ok(),
        Value::Null => return Err("the action needs `target`: an element number or [x, y]".into()),
        _ => None,
    }
    .ok_or_else(|| format!("`target` must be an element number or [x, y], not {value}"))? as u32;
    let state = tab.state.lock().unwrap();
    match state.numbers.node(number) {
        Some(node) => Ok(Target::Element(number, node)),
        None if state.numbers.from_earlier_document(number) => {
            Err(format!("element [{number}] was on the page before it loaded again, and nothing like it is there now; the state below has the new numbers"))
        }
        None => Err(format!("there is no element [{number}] on the page now; the state below shows the page")),
    }
}

/// The point to press: an element's middle after scrolling it into view, if nothing covers it.
async fn point(tab: &Tab, target: &Target) -> Result<(f64, f64), String> {
    let &Target::Element(number, node) = target else {
        let Target::Point(x, y) = *target else { unreachable!() };
        return Ok((x, y));
    };
    let gone = |e: String| match e.contains("No node") || e.contains("not found") || e.contains("detached") {
        true => format!("element [{number}] is no longer on the page; the state below shows the page now"),
        false => format!("element [{number}]: {e}"),
    };
    tab.call("DOM.scrollIntoViewIfNeeded", json!({ "backendNodeId": node })).await.map_err(gone)?;
    let quads = tab.call("DOM.getContentQuads", json!({ "backendNodeId": node })).await.map_err(gone)?;
    let quad = quads["quads"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|q| q.as_array().map(|q| q.iter().filter_map(Value::as_f64).collect::<Vec<_>>()))
        .find(|q| q.len() == 8 && area(q) >= 1.0)
        .ok_or_else(|| format!("element [{number}] is not visible: it has no box on the page"))?;
    let x = (quad[0] + quad[2] + quad[4] + quad[6]) / 4.0;
    let y = (quad[1] + quad[3] + quad[5] + quad[7]) / 4.0;
    let hit = tab
        .call("DOM.getNodeForLocation", json!({ "x": x.round(), "y": y.round(), "ignorePointerEventsNone": true }))
        .await?;
    let Some(hit) = hit["backendNodeId"].as_i64().filter(|h| *h != node) else {
        return Ok((x, y));
    };
    let related = "function(hit) { return this === hit || this.contains(hit) || hit.contains(this) || \
(this.getRootNode() instanceof ShadowRoot && this.getRootNode().host.contains(hit)); }";
    if on_node(tab, node, related, json!([{ "objectId": object(tab, hit).await? }])).await? == json!(true) {
        return Ok((x, y));
    }
    let known = tab.state.lock().unwrap().numbers.number(hit);
    let cover = match known {
        Some(n) => format!("[{n}]"),
        None => describe(tab, hit).await,
    };
    Err(format!("element [{number}] is covered by {cover} at [{}, {}]", x.round(), y.round()))
}

fn area(q: &[f64]) -> f64 {
    ((q[0] * q[3] - q[2] * q[1]) + (q[2] * q[5] - q[4] * q[3]) + (q[4] * q[7] - q[6] * q[5]) + (q[6] * q[1] - q[0] * q[7])).abs() / 2.0
}

async fn object(tab: &Tab, node: i64) -> Result<String, String> {
    let resolved = tab.call("DOM.resolveNode", json!({ "backendNodeId": node })).await?;
    resolved["object"]["objectId"].as_str().map(str::to_owned).ok_or_else(|| "the element is gone".into())
}

/// Calls `function` with the element as `this` and returns its value.
async fn on_node(tab: &Tab, node: i64, function: &str, arguments: Value) -> Result<Value, String> {
    let object = object(tab, node).await?;
    let result = tab
        .call(
            "Runtime.callFunctionOn",
            json!({ "objectId": object, "functionDeclaration": function, "arguments": arguments, "returnByValue": true }),
        )
        .await?;
    if let Some(details) = result.get("exceptionDetails") {
        return Err(details["exception"]["description"].as_str().unwrap_or("the script failed").to_owned());
    }
    Ok(result["result"]["value"].clone())
}

/// `<tag#id.class> "text"` of an element without a number.
async fn describe(tab: &Tab, node: i64) -> String {
    let function = "function() { const e = this.nodeType === 1 ? this : this.parentElement; if (!e) return 'a node'; \
let s = '<' + e.tagName.toLowerCase() + (e.id ? '#' + e.id : '') + (typeof e.className === 'string' && e.className.trim() ? '.' + e.className.trim().split(/\\s+/).slice(0, 2).join('.') : '') + '>'; \
const t = (e.innerText || '').trim().replace(/\\s+/g, ' '); return t ? s + ' \"' + t.slice(0, 60) + '\"' : s; }";
    on_node(tab, node, function, json!([])).await.ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_else(|| "another element".into())
}

async fn mouse(tab: &Tab, kind: &str, x: f64, y: f64, button: &str, count: u32, modifiers: u32) -> Result<(), String> {
    let buttons = match (kind, button) {
        ("mouseReleased", _) | (_, "none") => 0,
        (_, "right") => 2,
        (_, "middle") => 4,
        _ => 1,
    };
    tab.call(
        "Input.dispatchMouseEvent",
        json!({ "type": kind, "x": x, "y": y, "button": button, "buttons": buttons, "clickCount": count, "modifiers": modifiers }),
    )
    .await
    .map(drop)
}

async fn click(tab: &Tab, action: &Value) -> Result<(), String> {
    let (x, y) = point(tab, &target(tab, &action["target"])?).await?;
    let button = action["button"].as_str().unwrap_or("left");
    if !matches!(button, "left" | "right" | "middle") {
        return Err(format!("`button` is left, right or middle, not `{button}`"));
    }
    let count = action["count"].as_u64().unwrap_or(1).clamp(1, 3) as u32;
    let modifiers = match &action["keys"] {
        Value::Null => 0,
        keys => combo(&keys_text(keys))?.0,
    };
    mouse(tab, "mouseMoved", x, y, "none", 0, modifiers).await?;
    for n in 1..=count {
        mouse(tab, "mousePressed", x, y, button, n, modifiers).await?;
        mouse(tab, "mouseReleased", x, y, button, n, modifiers).await?;
    }
    Ok(())
}

/// Modifier keys given as "ctrl+shift" or ["ctrl", "shift"], as one combo text.
fn keys_text(keys: &Value) -> String {
    match keys {
        Value::Array(list) => list.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("+"),
        other => other.as_str().unwrap_or_default().to_owned(),
    }
}

async fn type_text(tab: &Tab, action: &Value) -> Result<(), String> {
    let text = action["text"].as_str().ok_or("`type` needs `text`")?;
    if !action["target"].is_null() {
        let target = target(tab, &action["target"])?;
        match target {
            Target::Element(_, node) => {
                if tab.call("DOM.focus", json!({ "backendNodeId": node })).await.is_err() {
                    let (x, y) = point(tab, &target).await?;
                    mouse(tab, "mousePressed", x, y, "left", 1, 0).await?;
                    mouse(tab, "mouseReleased", x, y, "left", 1, 0).await?;
                }
                let to_end = "function() { if (typeof this.setSelectionRange === 'function') { try { const n = this.value.length; this.setSelectionRange(n, n); } catch {} } }";
                let _ = on_node(tab, node, to_end, json!([])).await;
            }
            Target::Point(x, y) => {
                mouse(tab, "mousePressed", x, y, "left", 1, 0).await?;
                mouse(tab, "mouseReleased", x, y, "left", 1, 0).await?;
            }
        }
    }
    tab.call("Input.insertText", json!({ "text": text })).await.map(drop)
}

/// Sets a field's value the way a person's input would, so page scripts see input and change events.
const FILL: &str = "function(v) {
  const e = this, text = typeof v === 'string' ? v : String(v);
  if (e instanceof HTMLSelectElement) {
    const want = text.trim().toLowerCase();
    const option = [...e.options].find(o => o.text.trim().toLowerCase() === want || o.value === text);
    if (!option) return 'no option ' + JSON.stringify(text) + '; options: ' + [...e.options].map(o => o.text.trim()).join(' | ');
    e.value = option.value;
  } else if (e instanceof HTMLInputElement && (e.type === 'checkbox' || e.type === 'radio')) {
    if (e.checked !== (v === true || v === 'true')) e.click();
    return '';
  } else if (e instanceof HTMLInputElement || e instanceof HTMLTextAreaElement) {
    e.focus();
    const proto = e instanceof HTMLInputElement ? HTMLInputElement.prototype : HTMLTextAreaElement.prototype;
    Object.getOwnPropertyDescriptor(proto, 'value').set.call(e, text);
  } else if (['checkbox', 'switch', 'radio', 'menuitemcheckbox'].includes(e.getAttribute('role'))) {
    const on = (e.getAttribute('aria-checked') || e.getAttribute('aria-pressed')) === 'true';
    return on === (v === true || v === 'true') ? '' : 'click';
  } else if (e.isContentEditable) {
    e.focus();
    document.execCommand('selectAll');
    document.execCommand('insertText', false, text);
    return '';
  } else {
    return 'not a field: <' + e.tagName.toLowerCase() + '>';
  }
  e.dispatchEvent(new Event('input', { bubbles: true }));
  e.dispatchEvent(new Event('change', { bubbles: true }));
  return '';
}";

async fn fill(tab: &Tab, action: &Value) -> Result<(), String> {
    let target = target(tab, &action["target"])?;
    let Target::Element(number, node) = target else {
        return Err("`fill` needs an element number in `target`".into());
    };
    let value = match &action["value"] {
        Value::Null => return Err("`fill` needs `value`".into()),
        value => value.clone(),
    };
    tab.call("DOM.scrollIntoViewIfNeeded", json!({ "backendNodeId": node })).await.ok();
    let result = on_node(tab, node, FILL, json!([{ "value": value }])).await?;
    match result.as_str().unwrap_or_default() {
        "" => Ok(()),
        "click" => click(tab, &json!({ "target": number })).await,
        problem => Err(format!("element [{number}]: {problem}")),
    }
}

/// Bits of modifier keys as DevTools counts them.
fn modifier(name: &str) -> Option<(u32, &'static str, &'static str, u32)> {
    match name.to_lowercase().as_str() {
        "alt" | "option" => Some((1, "Alt", "AltLeft", 18)),
        "ctrl" | "control" => Some((2, "Control", "ControlLeft", 17)),
        "meta" | "cmd" | "command" | "win" | "super" => Some((4, "Meta", "MetaLeft", 91)),
        "shift" => Some((8, "Shift", "ShiftLeft", 16)),
        _ => None,
    }
}

/// Modifier bits and the main key of "ctrl+shift+Tab"; the key may be missing.
fn combo(text: &str) -> Result<(u32, Option<String>), String> {
    let mut bits = 0;
    let mut key = None;
    let parts: Vec<&str> = if text == "+" { vec!["+"] } else { text.split('+').filter(|p| !p.is_empty()).collect() };
    for part in parts {
        match modifier(part) {
            Some((bit, ..)) => bits |= bit,
            None if key.is_none() => key = Some(part.to_owned()),
            None => return Err(format!("`{text}` has two keys that are not modifiers")),
        }
    }
    Ok((bits, key))
}

/// key, code, Windows key code and typed text of a key name.
fn key_info(name: &str, shift: bool) -> Result<(String, String, u32, String), String> {
    let named = |key: &str, code: &str, vk: u32, text: &str| Ok((key.into(), code.into(), vk, text.into()));
    let lower = name.to_lowercase();
    match lower.as_str() {
        "enter" | "return" => return named("Enter", "Enter", 13, "\r"),
        "tab" => return named("Tab", "Tab", 9, ""),
        "escape" | "esc" => return named("Escape", "Escape", 27, ""),
        "backspace" => return named("Backspace", "Backspace", 8, ""),
        "delete" | "del" => return named("Delete", "Delete", 46, ""),
        "space" | " " => return named(" ", "Space", 32, " "),
        "arrowup" | "up" => return named("ArrowUp", "ArrowUp", 38, ""),
        "arrowdown" | "down" => return named("ArrowDown", "ArrowDown", 40, ""),
        "arrowleft" | "left" => return named("ArrowLeft", "ArrowLeft", 37, ""),
        "arrowright" | "right" => return named("ArrowRight", "ArrowRight", 39, ""),
        "home" => return named("Home", "Home", 36, ""),
        "end" => return named("End", "End", 35, ""),
        "pageup" => return named("PageUp", "PageUp", 33, ""),
        "pagedown" => return named("PageDown", "PageDown", 34, ""),
        "insert" => return named("Insert", "Insert", 45, ""),
        _ => {}
    }
    if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()).filter(|n| (1..=12).contains(n)) {
        let key = format!("F{n}");
        return Ok((key.clone(), key, 111 + n, String::new()));
    }
    let mut chars = name.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return Err(format!(
            "unknown key `{name}`; keys: a single character, Enter, Tab, Escape, Backspace, Delete, Space, arrows, Home, End, PageUp, PageDown, F1-F12"
        ));
    };
    let typed = if shift { c.to_uppercase().collect() } else { c.to_string() };
    Ok(match c.to_ascii_uppercase() {
        u @ 'A'..='Z' => (typed.clone(), format!("Key{u}"), u as u32, typed),
        d @ '0'..='9' => (typed.clone(), format!("Digit{d}"), d as u32, typed),
        _ => (typed.clone(), String::new(), 0, typed),
    })
}

/// Editing commands for shortcuts a headless browser does not run by itself.
fn command(bits: u32, key: &str) -> Option<&'static str> {
    if bits & (2 | 4) == 0 {
        return None;
    }
    Some(match (key.to_lowercase().as_str(), bits & 8 != 0) {
        ("a", _) => "selectAll",
        ("c", _) => "copy",
        ("x", _) => "cut",
        ("v", _) => "paste",
        ("z", false) => "undo",
        ("z", true) | ("y", _) => "redo",
        _ => return None,
    })
}

async fn press(tab: &Tab, keys: &str) -> Result<(), String> {
    let (bits, key) = combo(keys)?;
    let key = key.ok_or_else(|| format!("`{keys}` has only modifiers; add a key"))?;
    let (key_name, code, vk, text) = key_info(&key, bits & 8 != 0)?;
    let held: Vec<_> = keys.split('+').filter_map(modifier).collect();
    let mut down = 0;
    for (bit, name, code, vk) in &held {
        down |= bit;
        let event = json!({ "type": "rawKeyDown", "key": name, "code": code, "windowsVirtualKeyCode": vk, "modifiers": down });
        tab.call("Input.dispatchKeyEvent", event).await?;
    }
    // A shortcut types nothing.
    let text = if bits & (1 | 2 | 4) == 0 { text } else { String::new() };
    let mut event = json!({
        "type": if text.is_empty() { "rawKeyDown" } else { "keyDown" },
        "key": key_name, "code": code, "windowsVirtualKeyCode": vk, "modifiers": bits, "text": text,
    });
    if let Some(command) = command(bits, &key) {
        event["commands"] = json!([command]);
    }
    tab.call("Input.dispatchKeyEvent", event).await?;
    let up = json!({ "type": "keyUp", "key": key_name, "code": code, "windowsVirtualKeyCode": vk, "modifiers": bits });
    tab.call("Input.dispatchKeyEvent", up).await?;
    for (bit, name, code, vk) in held.iter().rev() {
        down &= !bit;
        let event = json!({ "type": "keyUp", "key": name, "code": code, "windowsVirtualKeyCode": vk, "modifiers": down });
        tab.call("Input.dispatchKeyEvent", event).await?;
    }
    Ok(())
}

async fn scroll(tab: &Tab, action: &Value) -> Result<(), String> {
    let (dx, dy) = (action["dx"].as_f64().unwrap_or(0.0), action["dy"].as_f64().unwrap_or(0.0));
    let target = match &action["target"] {
        Value::Null => None,
        value => Some(target(tab, value)?),
    };
    if dx == 0.0 && dy == 0.0 {
        return match target {
            Some(Target::Element(number, node)) => tab
                .call("DOM.scrollIntoViewIfNeeded", json!({ "backendNodeId": node }))
                .await
                .map(drop)
                .map_err(|e| format!("element [{number}]: {e}")),
            _ => Err("`scroll` needs `dy`/`dx` in pixels, or an element `target` to bring into view".into()),
        };
    }
    let (x, y) = match target {
        Some(target) => point(tab, &target).await?,
        None => {
            let metrics = tab.call("Page.getLayoutMetrics", json!({})).await?;
            let view = &metrics["cssLayoutViewport"];
            (view["clientWidth"].as_f64().unwrap_or(800.0) / 2.0, view["clientHeight"].as_f64().unwrap_or(600.0) / 2.0)
        }
    };
    let wheel = json!({ "type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy });
    tab.call("Input.dispatchMouseEvent", wheel).await.map(drop)
}

async fn drag(tab: &Tab, action: &Value) -> Result<(), String> {
    let (x0, y0) = point(tab, &target(tab, &action["from"]).map_err(|e| format!("`from`: {e}"))?).await?;
    let (x1, y1) = point(tab, &target(tab, &action["to"]).map_err(|e| format!("`to`: {e}"))?).await?;
    mouse(tab, "mouseMoved", x0, y0, "none", 0, 0).await?;
    mouse(tab, "mousePressed", x0, y0, "left", 1, 0).await?;
    const STEPS: u32 = 10;
    for step in 1..=STEPS {
        let t = step as f64 / STEPS as f64;
        let move_to = json!({ "type": "mouseMoved", "x": x0 + (x1 - x0) * t, "y": y0 + (y1 - y0) * t, "button": "left", "buttons": 1 });
        tab.call("Input.dispatchMouseEvent", move_to).await?;
    }
    mouse(tab, "mouseReleased", x1, y1, "left", 1, 0).await
}

async fn upload(tab: &Tab, action: &Value, directory: &Path) -> Result<(), String> {
    let Target::Element(number, node) = target(tab, &action["target"])? else {
        return Err("`upload` needs the file field's element number in `target`".into());
    };
    let paths = crate::tools::str_list(action, "paths");
    if paths.is_empty() {
        return Err("`upload` needs `paths`".into());
    }
    let mut files = Vec::new();
    for path in paths {
        let full = resolve(directory, &path);
        if !full.is_file() {
            return Err(format!("{} is not a file", full.display()));
        }
        files.push(full.to_string_lossy().into_owned());
    }
    tab.call("DOM.setFileInputFiles", json!({ "files": files, "backendNodeId": node }))
        .await
        .map(drop)
        .map_err(|e| format!("element [{number}]: {e}"))
}

async fn dialog(tab: &Tab, action: &Value) -> Result<(), String> {
    if tab.state.lock().unwrap().dialog.is_none() {
        return Err("no dialog is open".into());
    }
    let accept = action["accept"].as_bool().ok_or("`dialog` needs `accept`: true or false")?;
    let mut params = json!({ "accept": accept });
    if let Some(text) = action["text"].as_str() {
        params["promptText"] = json!(text);
    }
    tab.call("Page.handleJavaScriptDialog", params).await?;
    tab.state.lock().unwrap().dialog = None;
    Ok(())
}

/// `localhost:3000` and other addresses without a scheme as http(s) URLs.
pub fn full_url(url: &str) -> String {
    let url = url.trim();
    if url.contains("://") || url.starts_with("about:") || url.starts_with("data:") {
        return url.to_owned();
    }
    let local = ["localhost", "127.", "0.0.0.0", "[::1]"].iter().any(|l| url.starts_with(l));
    format!("{}://{url}", if local { "http" } else { "https" })
}

pub async fn goto(tab: &Tab, url: &str) -> Result<(), String> {
    let url = full_url(url);
    let result = tab.call("Page.navigate", json!({ "url": url })).await?;
    match result["errorText"].as_str().filter(|e| !e.is_empty()) {
        Some(error) => Err(format!("opening {url} failed: {error}")),
        None => Ok(()),
    }
}

pub async fn history(tab: &Tab, step: i64) -> Result<(), String> {
    let history = tab.call("Page.getNavigationHistory", json!({})).await?;
    let index = history["currentIndex"].as_i64().unwrap_or(0) + step;
    let entry = history["entries"]
        .as_array()
        .and_then(|e| e.get(usize::try_from(index).ok()?))
        .ok_or(if step < 0 { "there is no page to go back to" } else { "there is no page to go forward to" })?;
    tab.call("Page.navigateToHistoryEntry", json!({ "entryId": entry["id"] })).await.map(drop)
}

async fn viewport(tab: &Tab, action: &Value) -> Result<(), String> {
    if action["width"].as_str() == Some("auto") {
        tab.state.lock().unwrap().fixed = false;
        tab.call("Emulation.setTouchEmulationEnabled", json!({ "enabled": false })).await?;
        if tab.state.lock().unwrap().panel_size.is_none() {
            return tab.call("Emulation.clearDeviceMetricsOverride", json!({})).await.map(drop);
        }
        return tab.fit_panel().await;
    }
    let mobile = action["width"].as_str() == Some("mobile");
    let (width, height) = match mobile {
        true => (390, 844),
        false => (
            action["width"].as_u64().ok_or("`viewport` needs `width` (or \"mobile\" or \"auto\") and `height`")?,
            action["height"].as_u64().ok_or("`viewport` needs `height`")?,
        ),
    };
    let metrics = json!({ "width": width, "height": height, "deviceScaleFactor": 1, "mobile": mobile });
    tab.call("Emulation.setDeviceMetricsOverride", metrics).await?;
    tab.state.lock().unwrap().fixed = true;
    tab.call("Emulation.setTouchEmulationEnabled", json!({ "enabled": mobile })).await.map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos_and_keys() {
        assert_eq!(combo("ctrl+shift+Tab").unwrap(), (10, Some("Tab".into())));
        assert_eq!(combo("+").unwrap(), (0, Some("+".into())));
        assert!(combo("a+b").is_err());
        assert_eq!(key_info("a", true).unwrap(), ("A".into(), "KeyA".into(), 65, "A".into()));
        assert_eq!(key_info("Enter", false).unwrap().2, 13);
        assert_eq!(command(2, "a"), Some("selectAll"));
        assert_eq!(full_url("localhost:5173/x"), "http://localhost:5173/x");
        assert_eq!(full_url("example.com"), "https://example.com");
    }
}
