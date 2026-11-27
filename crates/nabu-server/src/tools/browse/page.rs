use std::collections::{HashMap, HashSet};

use regex::RegexBuilder;
use serde_json::Value;

use crate::tools::web::{clip, squeeze};

/// Lines a state shows before it is cut.
pub const STATE_LINES: usize = 400;
/// States up to this many lines come whole, with their changes marked.
pub const SHORT_STATE: usize = 40;
const NAME_CHARS: usize = 120;
const TEXT_CHARS: usize = 150;

/// Roles that get a number: things to click, type into or pick.
const INTERACTIVE: &[&str] = &[
    "button", "link", "textbox", "searchbox", "checkbox", "radio", "combobox", "listbox", "option", "menuitem",
    "menuitemcheckbox", "menuitemradio", "tab", "switch", "slider", "spinbutton", "treeitem", "textField",
];

/// Containers shown only through their content when they have no name.
const WRAPPERS: &[&str] = &[
    "generic", "none", "group", "paragraph", "list", "listitem", "section", "Section", "div", "LayoutTable",
    "LayoutTableRow", "LayoutTableCell", "emphasis", "strong", "code", "time", "mark", "LabelText", "label",
    "blockquote", "figure", "Abbr", "DescriptionList", "term", "definition", "Pre", "superscript", "subscript",
    "insertion", "deletion", "ruby", "Ruby", "RootWebArea", "WebArea", "presentation", "sectionheader", "sectionfooter",
    "MenuListPopup",
];

/// What makes a new element take a gone element's number: role, name and place among elements with both.
type Key = (String, String, usize);

/// Short numbers for elements. An element keeps its number while it stays on the page; an element that
/// replaces it, after a re-render or a reload, takes the number when its role, name and place match.
#[derive(Default)]
pub struct Numbers {
    by_node: HashMap<i64, u32>,
    by_number: HashMap<u32, i64>,
    by_key: HashMap<Key, u32>,
    next: u32,
    /// Numbers up to this one were given on earlier documents of the tab.
    earlier: u32,
}

impl Numbers {
    /// Numbers for the elements of one state, in page order.
    fn assign(&mut self, elements: &[(i64, Key)]) -> Vec<u32> {
        let present: HashSet<i64> = elements.iter().map(|(node, _)| *node).collect();
        let mut out = Vec::with_capacity(elements.len());
        for (node, key) in elements {
            let gone = |number: &u32| self.by_number.get(number).is_none_or(|old| !present.contains(old));
            let (known, reuse) = (self.by_node.get(node).copied(), self.by_key.get(key).copied().filter(gone));
            let number = match (known, reuse) {
                (Some(number), _) => number,
                (None, Some(number)) => {
                    if let Some(old) = self.by_number.remove(&number) {
                        self.by_node.remove(&old);
                    }
                    number
                }
                (None, None) => {
                    self.next += 1;
                    self.next
                }
            };
            self.by_node.insert(*node, number);
            self.by_number.insert(number, *node);
            self.by_key.insert(key.clone(), number);
            out.push(number);
        }
        out
    }

    /// The DOM node of an element number.
    pub fn node(&self, number: u32) -> Option<i64> {
        self.by_number.get(&number).copied()
    }

    pub fn number(&self, node: i64) -> Option<u32> {
        self.by_node.get(&node).copied()
    }

    /// A new document: its elements get numbers on the next state, by key from the old ones.
    pub fn forget_nodes(&mut self) {
        self.by_node.clear();
        self.by_number.clear();
        self.earlier = self.next;
    }

    pub fn from_earlier_document(&self, number: u32) -> bool {
        number <= self.earlier
    }
}

/// Numbered elements found while walking: line index, DOM node, key; and how many of each role and name came so far.
#[derive(Default)]
struct Found {
    elements: Vec<(usize, i64, Key)>,
    seen: HashMap<(String, String), usize>,
    /// The line of the focused element.
    focus: Option<usize>,
}

/// One state of a page.
pub struct Rendered {
    pub title: String,
    pub lines: Vec<String>,
    /// The focused element: its number, or its line when it has none.
    pub focus: Option<String>,
}

/// The page title and its state lines from `Accessibility.getFullAXTree` nodes.
pub fn render(nodes: &[Value], numbers: &mut Numbers) -> Rendered {
    let by_id: HashMap<&str, &Value> = nodes.iter().filter_map(|n| Some((n["nodeId"].as_str()?, n))).collect();
    let Some(root) = nodes.iter().find(|n| n["parentId"].is_null()) else {
        return Rendered { title: String::new(), lines: Vec::new(), focus: None };
    };
    let title = squeeze(root["name"]["value"].as_str().unwrap_or(""));
    let mut lines = Vec::new();
    let mut found = Found::default();
    walk(root, &by_id, 0, "", &mut found, &mut lines);
    let keys: Vec<(i64, Key)> = found.elements.iter().map(|(_, node, key)| (*node, key.clone())).collect();
    for ((line, _, _), number) in found.elements.iter().zip(numbers.assign(&keys)) {
        let at = depth_of(&lines[*line]);
        lines[*line].insert_str(at, &format!("[{number}] "));
    }
    let focus = found.focus.map(|line| {
        let line = lines[line].trim_start();
        match line.strip_prefix('[').and_then(|l| l.split_once(']')) {
            Some((number, _)) => format!("[{number}]"),
            None => line.to_owned(),
        }
    });
    Rendered { title, lines: without_labels(lines), focus }
}

/// Drops a label's text right before or after the field it names: the field's line has the same name.
fn without_labels(lines: Vec<String>) -> Vec<String> {
    let names = |text: &str, field: Option<&String>| {
        let Some(field) = field else { return false };
        depth_of(text) == depth_of(field)
            && field.trim_start().starts_with('[')
            && text.trim_start().strip_prefix("text ").is_some_and(|name| field.contains(&format!(" {name}")))
    };
    let mut out = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        let before = i.checked_sub(1).and_then(|j| lines.get(j));
        if !names(line, lines.get(i + 1)) && !names(line, before) {
            out.push(line.clone());
        }
    }
    out
}

fn walk(node: &Value, by_id: &HashMap<&str, &Value>, depth: usize, context: &str, found: &mut Found, out: &mut Vec<String>) {
    let role = node["role"]["value"].as_str().unwrap_or("");
    let name = squeeze(node["name"]["value"].as_str().unwrap_or(""));
    let children = |depth: usize, context: &str, found: &mut Found, out: &mut Vec<String>| {
        for id in node["childIds"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if let Some(child) = by_id.get(id) {
                walk(child, by_id, depth, context, found, out);
            }
        }
    };
    if node["ignored"].as_bool().unwrap_or(false) {
        return children(depth, context, found, out);
    }
    match role {
        "InlineTextBox" | "LineBreak" | "ListMarker" => return,
        "StaticText" => {
            if !name.is_empty() && !context.contains(&name) {
                out.push(format!("{}text \"{}\"", indent(depth), clip(&name, TEXT_CHARS)));
            }
            return;
        }
        "img" | "image" if name.is_empty() => return,
        _ => {}
    }
    let props = properties(node);
    let interactive = INTERACTIVE.contains(&role)
        || (props.contains_key("focusable") && !matches!(role, "RootWebArea" | "WebArea" | "Iframe" | "iframe"))
        || props.get("editable").is_some_and(|e| e == "richtext" || e == "plaintext") && props.contains_key("focusable");
    if !interactive && name.is_empty() && WRAPPERS.contains(&role) {
        return children(depth, context, found, out);
    }
    if role == "RootWebArea" {
        return children(depth, &name, found, out);
    }
    if interactive && let Some(node_id) = node["backendDOMNodeId"].as_i64() {
        let place = found.seen.entry((role.to_owned(), name.clone())).or_default();
        found.elements.push((out.len(), node_id, (role.to_owned(), name.clone(), *place)));
        *place += 1;
    }
    if props.get("focused").is_some_and(|f| f == "true") {
        found.focus = Some(out.len());
    }
    let mut line = indent(depth);
    line.push_str(role);
    if !name.is_empty() {
        line.push_str(&format!(" \"{}\"", clip(&name, NAME_CHARS)));
    }
    let value = squeeze(node["value"]["value"].as_str().unwrap_or(""));
    if !value.is_empty() && value != name && !matches!(role, "link") {
        line.push_str(&format!(" = \"{}\"", clip(&value, NAME_CHARS)));
    }
    for state in states(&props) {
        line.push(' ');
        line.push_str(&state);
    }
    out.push(line);
    // A field's text repeats its value, a named element's text repeats its name.
    let shown = format!("{name} {value}");
    children(depth + 1, if shown.trim().is_empty() { context } else { &shown }, found, out);
}

fn indent(depth: usize) -> String {
    "  ".repeat(depth)
}

fn properties(node: &Value) -> HashMap<String, String> {
    node["properties"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let value = match &p["value"]["value"] {
                Value::String(s) => s.clone(),
                Value::Null => return None,
                other => other.to_string(),
            };
            Some((p["name"].as_str()?.to_owned(), value))
        })
        .collect()
}

/// Words for the element's states, as a person would see them.
fn states(props: &HashMap<String, String>) -> Vec<String> {
    let on = |key: &str| props.get(key).is_some_and(|v| v == "true");
    let mut out = Vec::new();
    if let Some(level) = props.get("level") {
        out.push(format!("level {level}"));
    }
    match props.get("checked").or(props.get("pressed")).map(String::as_str) {
        Some("true") => out.push("checked".into()),
        Some("mixed") => out.push("mixed".into()),
        _ => {}
    }
    match props.get("expanded").map(String::as_str) {
        Some("true") => out.push("expanded".into()),
        Some("false") => out.push("collapsed".into()),
        _ => {}
    }
    for (key, word) in [("selected", "selected"), ("disabled", "disabled"), ("required", "required"), ("readonly", "readonly")] {
        if on(key) {
            out.push(word.into());
        }
    }
    if props.get("invalid").is_some_and(|v| v != "false") {
        out.push("invalid".into());
    }
    out
}

/// The lines matching `pattern`, each with the lines it sits in.
pub fn find(lines: &[String], pattern: &str) -> Result<Vec<String>, String> {
    let regex = RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .map_err(|e| format!("bad regular expression in `find`: {e}"))?;
    let mut keep = vec![false; lines.len()];
    for (i, line) in lines.iter().enumerate() {
        if !regex.is_match(line) {
            continue;
        }
        keep[i] = true;
        let mut depth = depth_of(line);
        for j in (0..i).rev() {
            if depth == 0 {
                break;
            }
            if depth_of(&lines[j]) < depth {
                keep[j] = true;
                depth = depth_of(&lines[j]);
            }
        }
    }
    Ok(lines.iter().zip(keep).filter(|(_, k)| *k).map(|(l, _)| l.clone()).collect())
}

fn depth_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Changed lines between two states, and how many stayed; `None` when most of it changed.
/// `- ` is a gone line, `+ ` a new one, `~ ` an element whose line changed while it kept its number.
/// `with_same` keeps the unchanged lines too, as `  ` lines.
pub fn changes(old: &[String], new: &[String], with_same: bool) -> Option<(Vec<String>, usize)> {
    let old: Vec<&str> = old.iter().map(String::as_str).collect();
    let new_lines: Vec<&str> = new.iter().map(String::as_str).collect();
    let diff = similar::TextDiff::from_slices(&old, &new_lines);
    let (mut out, mut same) = (Vec::new(), 0);
    let (mut gone, mut added) = (Vec::new(), Vec::new());
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Delete => gone.push(change.value()),
            similar::ChangeTag::Insert => added.push(change.value()),
            similar::ChangeTag::Equal => {
                pair_up(&mut gone, &mut added, &mut out);
                same += 1;
                if with_same {
                    out.push(format!("  {}", change.value()));
                }
            }
        }
    }
    pair_up(&mut gone, &mut added, &mut out);
    let changed = out.iter().filter(|line| !line.starts_with("  ")).count();
    if changed == 0 {
        out.clear();
    }
    (changed < new.len()).then_some((out, same))
}

/// One run of gone and added lines: an element in both, by number and depth, becomes one `~ ` line.
fn pair_up(gone: &mut Vec<&str>, added: &mut Vec<&str>, out: &mut Vec<String>) {
    let key = |line: &str| {
        let number = line.trim_start().strip_prefix('[')?.split_once(']')?.0.to_owned();
        Some((depth_of(line), number))
    };
    let added_keys: Vec<_> = added.iter().filter_map(|l| key(l)).collect();
    let gone_keys: Vec<_> = gone.iter().filter_map(|l| key(l)).collect();
    for line in gone.drain(..) {
        if !key(line).is_some_and(|k| added_keys.contains(&k)) {
            out.push(format!("- {line}"));
        }
    }
    for line in added.drain(..) {
        let mark = if key(line).is_some_and(|k| gone_keys.contains(&k)) { '~' } else { '+' };
        out.push(format!("{mark} {line}"));
    }
}

/// At most `STATE_LINES`, with a note when cut.
pub fn cut(mut lines: Vec<String>) -> Vec<String> {
    if lines.len() > STATE_LINES {
        let total = lines.len();
        lines.truncate(STATE_LINES);
        lines.push(format!("(cut at {STATE_LINES} of {total} lines; `find` shows the lines you need)"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn renders_numbers_and_skips_wrappers() {
        let nodes = json!([
            { "nodeId": "1", "role": { "value": "RootWebArea" }, "name": { "value": "Sign in" }, "childIds": ["2", "3", "6"] },
            { "nodeId": "2", "parentId": "1", "role": { "value": "generic" }, "childIds": ["4"] },
            { "nodeId": "3", "parentId": "1", "role": { "value": "heading" }, "name": { "value": "Welcome" }, "childIds": ["5"],
              "properties": [{ "name": "level", "value": { "value": 1 } }] },
            { "nodeId": "4", "parentId": "2", "role": { "value": "button" }, "name": { "value": "Go" }, "backendDOMNodeId": 40,
              "properties": [{ "name": "focusable", "value": { "value": true } }, { "name": "disabled", "value": { "value": true } }] },
            { "nodeId": "5", "parentId": "3", "role": { "value": "StaticText" }, "name": { "value": "Welcome" } },
            { "nodeId": "6", "parentId": "1", "role": { "value": "StaticText" }, "name": { "value": "Use your  account" } }
        ]);
        let mut numbers = Numbers::default();
        let Rendered { title, lines, .. } = render(nodes.as_array().unwrap(), &mut numbers);
        assert_eq!(title, "Sign in");
        assert_eq!(lines, ["[1] button \"Go\" disabled", "heading \"Welcome\" level 1", "text \"Use your account\""]);
        assert_eq!(numbers.node(1), Some(40));
    }

    #[test]
    fn replaced_elements_take_gone_numbers() {
        let page = |boxes: &[(i64, &str)]| {
            let mut nodes = vec![json!({ "nodeId": "r", "role": { "value": "RootWebArea" }, "childIds": boxes.iter().map(|(n, _)| n.to_string()).collect::<Vec<_>>() })];
            nodes.extend(boxes.iter().map(|(node, name)| json!({
                "nodeId": node.to_string(), "parentId": "r", "role": { "value": "checkbox" }, "name": { "value": name }, "backendDOMNodeId": node
            })));
            nodes
        };
        let mut numbers = Numbers::default();
        render(&page(&[(10, "Milk"), (11, "Bread"), (12, "Delete"), (13, "Delete")]), &mut numbers);
        let lines = render(&page(&[(20, "Milk"), (21, "Bread"), (22, "Delete"), (23, "Delete")]), &mut numbers).lines;
        assert_eq!(lines, ["[1] checkbox \"Milk\"", "[2] checkbox \"Bread\"", "[3] checkbox \"Delete\"", "[4] checkbox \"Delete\""]);
        numbers.forget_nodes();
        let lines = render(&page(&[(30, "Bread"), (31, "Eggs"), (32, "Delete")]), &mut numbers).lines;
        assert_eq!(lines, ["[2] checkbox \"Bread\"", "[5] checkbox \"Eggs\"", "[3] checkbox \"Delete\""]);
        assert_eq!(numbers.node(1), None);
        assert!(numbers.from_earlier_document(1));
    }

    #[test]
    fn find_keeps_parents_and_changes_diff() {
        let lines: Vec<String> = ["form \"Login\"", "  [1] textbox \"Email\"", "  [2] button \"Go\"", "text \"x\""].map(String::from).into();
        assert_eq!(find(&lines, "go").unwrap(), ["form \"Login\"", "  [2] button \"Go\""]);
        let mut new = lines.clone();
        new[2] = "  [2] button \"Go\" disabled".into();
        let (diff, same) = changes(&lines, &new, false).unwrap();
        assert_eq!(diff, ["~   [2] button \"Go\" disabled"]);
        assert_eq!(same, 3);
        let (whole, _) = changes(&lines, &new, true).unwrap();
        assert_eq!(whole.len(), 4);
        assert_eq!(whole[0], "  form \"Login\"");
    }
}
