//! The tools that ask a language server.
//! What it reports about a file.
//! What it says about the symbol at a position.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::lsp::LspPool;
use crate::lsp::{check, position};
use crate::sandbox::allowlist;
use crate::tools::registry::{Tool, ToolCtx, ToolOutput};

/// How many diagnostics the tool lists before it counts the rest.
const LISTED: usize = 50;

pub struct LspDiagnostics {
    pub pool: Arc<LspPool>,
}

#[derive(Deserialize)]
struct DiagArgs {
    path: PathBuf,
}

#[async_trait]
impl Tool for LspDiagnostics {
    fn name(&self) -> &str {
        "lsp_diagnostics"
    }
    fn description(&self) -> &str {
        "Return current LSP diagnostics (errors/warnings) for a source file."
    }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        })
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: DiagArgs = serde_json::from_value(args)?;
        let path = match allowlist::resolve(&ctx.root, &a.path, true) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(allowlist::refusal(&ctx.root, &a.path, &e))),
        };
        let server = match self.pool.server_for(&path).await {
            Ok(s) => s,
            Err(why) => return Ok(ToolOutput::err(why)),
        };
        let asked = std::time::Instant::now();
        let target = match server.sync(&path).await {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolOutput::err(format!(
                    "could not reach the language server: {e:#}"
                )))
            }
        };
        let settled = check::settle(
            &server,
            std::slice::from_ref(&target),
            asked,
            check::SETTLE_AT_MOST,
        )
        .await;
        if let (Err(why), false) = (&settled, server.alive()) {
            return Ok(ToolOutput::err(why.clone()));
        }
        let shown = ctx.short(&path);
        let Some(published) = server.published(&target.0).await else {
            return Ok(match settled {
                Ok(()) => ToolOutput::ok(format!("{shown}: no diagnostics")),
                Err(why) => ToolOutput::err(why),
            });
        };
        let mut diags = published.diagnostics;
        diags.sort_by_key(|d| {
            (
                d.severity.map(|s| format!("{s:?}")) != Some("Error".into()),
                d.range.start.line,
            )
        });
        let mut out = Vec::new();
        for d in diags.iter().take(LISTED) {
            out.push(check::describe(&server, &ctx.root, &target.0, d).await);
        }
        if diags.len() > LISTED {
            out.push(format!("and {} more", diags.len() - LISTED));
        }
        if out.is_empty() {
            out.push(format!("{shown}: no diagnostics"));
        }
        if let Err(why) = settled {
            out.push(format!("this may be incomplete: {why}"));
        }
        Ok(ToolOutput::ok(out.join("\n")))
    }
}

/// What a tool that answers about one symbol asks of the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Definition,
    References,
    Implementations,
    Hover,
    Calls,
}

/// A tool that answers about the symbol at a place in a file.
pub struct LspAt {
    pub pool: Arc<LspPool>,
    pub op: Op,
}

impl LspAt {
    /// The 5 tools that answer about one symbol.
    pub fn all(pool: &Arc<LspPool>) -> Vec<Self> {
        [
            Op::Definition,
            Op::References,
            Op::Implementations,
            Op::Hover,
            Op::Calls,
        ]
        .into_iter()
        .map(|op| Self {
            pool: pool.clone(),
            op,
        })
        .collect()
    }
}

#[derive(Deserialize)]
struct AtArgs {
    path: PathBuf,
    /// 1-indexed line.
    line: u32,
    /// The symbol as it is written on that line.
    #[serde(default)]
    symbol: Option<String>,
    /// 1-indexed column, in characters, when the symbol is not named.
    #[serde(default)]
    column: Option<u32>,
    /// For calls: "incoming" or "outgoing".
    #[serde(default)]
    direction: Option<String>,
}

#[async_trait]
impl Tool for LspAt {
    fn name(&self) -> &str {
        match self.op {
            Op::Definition => "lsp_definition",
            Op::References => "lsp_references",
            Op::Implementations => "lsp_implementations",
            Op::Hover => "lsp_hover",
            Op::Calls => "lsp_calls",
        }
    }
    fn description(&self) -> &str {
        match self.op {
            Op::Definition => {
                "Where a symbol is defined, from the language server. Name the file, the line \
                 and the symbol as it is written on that line."
            }
            Op::References => {
                "Every place a symbol is used, from the language server. Prefer it to grep to \
                 find the uses of a function, type, field or variable."
            }
            Op::Implementations => {
                "The implementations of a trait, an interface or an abstract method."
            }
            Op::Hover => "The type and the documentation of a symbol, from the language server.",
            Op::Calls => {
                "The functions that call a function (incoming), or that it calls (outgoing)."
            }
        }
    }
    fn args_schema(&self) -> Value {
        let mut props = json!({
            "path":   {"type": "string"},
            "line":   {"type": "integer", "minimum": 1},
            "symbol": {"type": "string", "description": "the name as written on that line"},
            "column": {"type": "integer", "minimum": 1}
        });
        if self.op == Op::Calls {
            props["direction"] = json!({"type": "string", "enum": ["incoming", "outgoing"]});
        }
        json!({
            "type": "object",
            "properties": props,
            "required": ["path", "line"],
            "additionalProperties": false
        })
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: AtArgs = serde_json::from_value(args)?;
        let at = match locate(&self.pool, ctx, &a).await {
            Ok(at) => at,
            Err(why) => return Ok(ToolOutput::err(why)),
        };
        let caps = at.server.caps();
        let (offered, what) = match self.op {
            Op::Definition => (caps.definition, "definitions"),
            Op::References => (caps.references, "references"),
            Op::Implementations => (caps.implementation, "implementations"),
            Op::Hover => (caps.hover, "hover"),
            Op::Calls => (caps.call_hierarchy, "call hierarchies"),
        };
        if !offered {
            return Ok(ToolOutput::err(format!(
                "{} does not answer {what}",
                at.server.spec.command
            )));
        }
        let position = json!({
            "textDocument": {"uri": at.uri},
            "position": {"line": at.line, "character": at.character}
        });
        let answer = match self.op {
            Op::Definition => at.server.request("textDocument/definition", position).await,
            Op::Implementations => {
                at.server
                    .request("textDocument/implementation", position)
                    .await
            }
            Op::Hover => at.server.request("textDocument/hover", position).await,
            Op::References => {
                let mut params = position;
                params["context"] = json!({"includeDeclaration": false});
                at.server.request("textDocument/references", params).await
            }
            Op::Calls => calls(&at, a.direction.as_deref()).await,
        };
        let answer = match answer {
            Ok(v) => v,
            Err(e) => return Ok(ToolOutput::err(format!("{e:#}"))),
        };
        let said = match self.op {
            Op::Hover => match answer.get("contents") {
                Some(c) => pretty_contents(c),
                None => format!("nothing is known of `{}`", at.name),
            },
            Op::Calls => answer.as_str().unwrap_or_default().to_string(),
            _ => {
                let found = locations(&answer);
                match found.is_empty() {
                    true => format!("the language server found no {what} of `{}`", at.name),
                    false => show_locations(&at.server, &ctx.root, &found).await,
                }
            }
        };
        let said = match &at.moved {
            Some(note) => format!("{note}\n{said}"),
            None => said,
        };
        Ok(ToolOutput::ok(said))
    }
}

/// A symbol found in a file: the server that answers for it and where it is, in the
/// server's units.
struct At {
    server: Arc<crate::lsp::server::Server>,
    uri: lsp_types::Url,
    line: u32,
    character: u32,
    name: String,
    /// Said when the symbol was found on another line than the one given.
    moved: Option<String>,
}

/// How far from the line given a named symbol is looked for.
const NEARBY_LINES: usize = 3;

/// Find the symbol `a` names, starting the server for its file.
async fn locate(pool: &LspPool, ctx: &ToolCtx, a: &AtArgs) -> Result<At, String> {
    let path = allowlist::resolve(&ctx.root, &a.path, true)
        .map_err(|e| allowlist::refusal(&ctx.root, &a.path, &e))?;
    let server = pool.server_for(&path).await?;
    let (uri, _, _) = server
        .sync(&path)
        .await
        .map_err(|e| format!("could not reach the language server: {e:#}"))?;
    let text = server.text_of(&path).await.unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let given = a.line.saturating_sub(1) as usize;
    let shown = ctx.short(&path);
    let (line, column, name, moved) = match (&a.symbol, a.column) {
        (Some(symbol), _) if !symbol.trim().is_empty() => {
            let symbol = symbol.trim();
            let (line, column) = find_symbol(&lines, given, symbol).ok_or_else(|| {
                format!(
                    "`{symbol}` is not on line {} of {shown} or near it. That line reads: {}",
                    a.line,
                    lines
                        .get(given)
                        .map(|l| l.trim())
                        .unwrap_or("(past the end)")
                )
            })?;
            let moved = (line != given).then(|| format!("(`{symbol}` is on line {})", line + 1));
            (line, column, symbol.to_string(), moved)
        }
        (_, Some(column)) => {
            let source = lines.get(given).copied().unwrap_or("");
            let column = column.saturating_sub(1) as usize;
            (given, column, word_at(source, column), None)
        }
        _ => return Err("name the symbol, as it is written on that line".into()),
    };
    let character = position::units(
        lines.get(line).copied().unwrap_or(""),
        column,
        server.caps().encoding,
    );
    Ok(At {
        server,
        uri,
        line: line as u32,
        character,
        name,
        moved,
    })
}

/// The line and character column of `symbol` as a whole word, on line `given` or the
/// nearest line to it that has it.
fn find_symbol(lines: &[&str], given: usize, symbol: &str) -> Option<(usize, usize)> {
    let nearby = std::iter::once(given).chain((1..=NEARBY_LINES).flat_map(|d| {
        [given.checked_sub(d), Some(given + d)]
            .into_iter()
            .flatten()
    }));
    for line in nearby {
        let Some(text) = lines.get(line) else {
            continue;
        };
        let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        for (at, _) in text.match_indices(symbol) {
            let before = text[..at].chars().next_back();
            let after = text[at + symbol.len()..].chars().next();
            if !word(before) && !word(after) {
                return Some((line, text[..at].chars().count()));
            }
        }
    }
    None
}

/// The word around character `column` of `line`.
fn word_at(line: &str, column: usize) -> String {
    let chars: Vec<char> = line.chars().collect();
    let is = |c: &char| c.is_alphanumeric() || *c == '_';
    let mut from = column.min(chars.len());
    while from > 0 && is(&chars[from - 1]) {
        from -= 1;
    }
    let mut to = column.min(chars.len());
    while to < chars.len() && is(&chars[to]) {
        to += 1;
    }
    chars[from..to].iter().collect()
}

/// Who calls the function at `at`, or what it calls, as text.
async fn calls(at: &At, direction: Option<&str>) -> Result<Value> {
    let items = at
        .server
        .request(
            "textDocument/prepareCallHierarchy",
            json!({
                "textDocument": {"uri": at.uri},
                "position": {"line": at.line, "character": at.character}
            }),
        )
        .await?;
    let Some(item) = items.as_array().and_then(|a| a.first()).cloned() else {
        return Ok(Value::String(format!(
            "`{}` is not a function the server knows",
            at.name
        )));
    };
    let outgoing = direction == Some("outgoing");
    let (method, side) = match outgoing {
        true => ("callHierarchy/outgoingCalls", "to"),
        false => ("callHierarchy/incomingCalls", "from"),
    };
    let found = at.server.request(method, json!({"item": item})).await?;
    let mut out = Vec::new();
    for call in found.as_array().into_iter().flatten().take(LISTED) {
        let Some(other) = call.get(side) else {
            continue;
        };
        let name = other.get("name").and_then(Value::as_str).unwrap_or("?");
        let place = locations(&json!({
            "uri": other.get("uri"),
            "range": other.get("selectionRange").or(other.get("range"))
        }));
        out.push(match place.first() {
            Some((uri, range)) => format!(
                "{name}  {}:{}",
                short_uri(uri, &at.server_root()),
                range.start.line + 1
            ),
            None => name.to_string(),
        });
    }
    let what = match outgoing {
        true => format!("`{}` calls", at.name),
        false => format!("`{}` is called by", at.name),
    };
    Ok(Value::String(match out.is_empty() {
        true => format!("{what} nothing the server knows of"),
        false => format!("{what}:\n{}", out.join("\n")),
    }))
}

impl At {
    fn server_root(&self) -> PathBuf {
        self.server.root().to_path_buf()
    }
}

/// The places a definition, implementation or references answer names.
fn locations(answer: &Value) -> Vec<(lsp_types::Url, lsp_types::Range)> {
    let one = |v: &Value| -> Option<(lsp_types::Url, lsp_types::Range)> {
        let uri = v.get("uri").or(v.get("targetUri"))?.as_str()?;
        let range = v.get("range").or(v.get("targetSelectionRange"))?;
        Some((
            lsp_types::Url::parse(uri).ok()?,
            serde_json::from_value(range.clone()).ok()?,
        ))
    };
    match answer {
        Value::Array(list) => list.iter().filter_map(one).collect(),
        Value::Object(_) => one(answer).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// A path as the project names it, or whole when it is outside the project.
fn short_uri(uri: &lsp_types::Url, root: &std::path::Path) -> String {
    let path = uri.to_file_path().unwrap_or_default();
    match path.strip_prefix(root) {
        Ok(rel) => rel.display().to_string(),
        Err(_) => format!("{} (outside the project)", path.display()),
    }
}

/// One line per place: where it is, in characters, and the source line there.
async fn show_locations(
    server: &crate::lsp::server::Server,
    root: &std::path::Path,
    found: &[(lsp_types::Url, lsp_types::Range)],
) -> String {
    let mut out = Vec::new();
    for (uri, range) in found.iter().take(LISTED) {
        let path = uri.to_file_path().unwrap_or_default();
        let text = server.text_of(&path).await.unwrap_or_default();
        let source = text.lines().nth(range.start.line as usize).unwrap_or("");
        let column = position::chars(source, range.start.character, server.caps().encoding) + 1;
        out.push(format!(
            "{}:{}:{column}  {}",
            short_uri(uri, root),
            range.start.line + 1,
            source.trim()
        ));
    }
    if found.len() > LISTED {
        out.push(format!("and {} more", found.len() - LISTED));
    }
    out.join("\n")
}

/// The outline of a file, or the symbols matching a name across the project.
pub struct LspSymbols {
    pub pool: Arc<LspPool>,
}

#[derive(Deserialize)]
struct SymbolArgs {
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    query: Option<String>,
}

#[async_trait]
impl Tool for LspSymbols {
    fn name(&self) -> &str {
        "lsp_symbols"
    }
    fn description(&self) -> &str {
        "With a path: the outline of that file, each type, function and field with its line. \
         With a query: the symbols whose name matches it across the project. Prefer it to \
         reading a whole file to find where something is."
    }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":  {"type": "string"},
                "query": {"type": "string"}
            },
            "additionalProperties": false
        })
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: SymbolArgs = serde_json::from_value(args)?;
        match (a.path, a.query) {
            (Some(path), _) => Ok(outline(&self.pool, ctx, &path).await),
            (None, Some(query)) => Ok(search(&self.pool, ctx, &query).await),
            (None, None) => Ok(ToolOutput::err("give a path for an outline, or a query")),
        }
    }
}

async fn outline(pool: &LspPool, ctx: &ToolCtx, path: &std::path::Path) -> ToolOutput {
    let path = match allowlist::resolve(&ctx.root, path, true) {
        Ok(p) => p,
        Err(e) => return ToolOutput::err(allowlist::refusal(&ctx.root, path, &e)),
    };
    let server = match pool.server_for(&path).await {
        Ok(s) => s,
        Err(why) => return ToolOutput::err(why),
    };
    if !server.caps().document_symbol {
        return ToolOutput::err(format!("{} gives no outline", server.spec.command));
    }
    let uri = match server.sync(&path).await {
        Ok((uri, _, _)) => uri,
        Err(e) => return ToolOutput::err(format!("could not reach the language server: {e:#}")),
    };
    let answer = match server
        .request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": uri}}),
        )
        .await
    {
        Ok(a) => a,
        Err(e) => return ToolOutput::err(format!("{e:#}")),
    };
    let mut out = Vec::new();
    for symbol in answer.as_array().into_iter().flatten() {
        outline_lines(symbol, 0, &mut out);
    }
    if out.is_empty() {
        return ToolOutput::ok(format!(
            "{} declares nothing the server lists",
            ctx.short(&path)
        ));
    }
    let total = out.len();
    out.truncate(OUTLINE_LISTED);
    if total > OUTLINE_LISTED {
        out.push(format!("and {} more", total - OUTLINE_LISTED));
    }
    ToolOutput::ok(out.join("\n"))
}

/// How many lines an outline lists before it counts the rest.
const OUTLINE_LISTED: usize = 200;

/// One line per symbol, nested under its parent: its line, its kind and its name.
fn outline_lines(symbol: &Value, depth: usize, out: &mut Vec<String>) {
    let name = symbol.get("name").and_then(Value::as_str).unwrap_or("?");
    let kind = symbol_kind(symbol.get("kind").and_then(Value::as_u64).unwrap_or(0));
    let line = symbol
        .get("selectionRange")
        .or(symbol.get("location").and_then(|l| l.get("range")))
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
        + 1;
    out.push(format!("{line:>5}  {}{kind} {name}", "  ".repeat(depth)));
    for child in symbol
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        outline_lines(child, depth + 1, out);
    }
}

async fn search(pool: &LspPool, ctx: &ToolCtx, query: &str) -> ToolOutput {
    let servers = pool.running().await;
    if servers.is_empty() {
        return ToolOutput::err(
            "no language server is running yet: ask about a file of the project first",
        );
    }
    let mut out = Vec::new();
    for server in servers.iter().filter(|s| s.caps().workspace_symbol) {
        let Ok(answer) = server
            .request("workspace/symbol", json!({"query": query}))
            .await
        else {
            continue;
        };
        for symbol in answer.as_array().into_iter().flatten() {
            let name = symbol.get("name").and_then(Value::as_str).unwrap_or("?");
            let kind = symbol_kind(symbol.get("kind").and_then(Value::as_u64).unwrap_or(0));
            let place = locations(symbol.get("location").unwrap_or(&Value::Null));
            let Some((uri, range)) = place.first() else {
                continue;
            };
            out.push(format!(
                "{kind} {name}  {}:{}",
                short_uri(uri, &ctx.root),
                range.start.line + 1
            ));
        }
    }
    if out.is_empty() {
        return ToolOutput::ok(format!("no symbol matches `{query}`"));
    }
    let total = out.len();
    out.truncate(LISTED);
    if total > LISTED {
        out.push(format!("and {} more", total - LISTED));
    }
    ToolOutput::ok(out.join("\n"))
}

/// The protocol's symbol kinds, as words.
fn symbol_kind(kind: u64) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "fn",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "variant",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type parameter",
        _ => "symbol",
    }
}

fn pretty_contents(c: &Value) -> String {
    // Hover contents can be a string, a {kind, value}, or an array.
    if let Some(s) = c.as_str() {
        return s.to_string();
    }
    if let Some(v) = c.get("value").and_then(|v| v.as_str()) {
        return v.to_string();
    }
    if let Some(arr) = c.as_array() {
        return arr
            .iter()
            .map(pretty_contents)
            .collect::<Vec<_>>()
            .join("\n");
    }
    c.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::harness::{Answer, Harness};
    use serde_json::json;

    /// A language server that answers the parts of the protocol these 2 tools use.
    ///
    /// It reports the document version back inside the diagnostic.
    /// The version tells whether an edit reached the server.
    const STUB: &str = r#"
import json, sys

def read():
    length = 0
    while True:
        line = sys.stdin.buffer.readline()
        if not line or line in (b"\r\n", b"\n"):
            break
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":")[1])
    if not length:
        return None
    return json.loads(sys.stdin.buffer.read(length))

def send(msg):
    body = json.dumps(msg).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

versions = {}
while True:
    msg = read()
    if msg is None:
        break
    method, mid = msg.get("method"), msg.get("id")
    if method == "initialize":
        # A request of the server's own, numbered like the client's first one: the client
        # has to answer it, and must not take it for the answer to its own request.
        send({"jsonrpc": "2.0", "id": mid, "method": "workspace/configuration",
              "params": {"items": [{"section": "stub"}]}})
        answer = read()
        config = answer.get("result") if answer and "method" not in answer else "unanswered"
        send({"jsonrpc": "2.0", "id": mid, "result": {"capabilities": {"hoverProvider": True}}})
    elif method in ("textDocument/didOpen", "textDocument/didChange"):
        doc = msg["params"]["textDocument"]
        text = doc.get("text") or msg["params"].get("contentChanges", [{}])[0].get("text", "")
        if "CRASH" in text:
            sys.stderr.write("stub crashed on purpose\n")
            sys.exit(1)
        versions[doc["uri"]] = doc.get("version", 0)
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {
            "uri": doc["uri"],
            "diagnostics": [{
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                "severity": 1,
                "message": "stub saw version %d" % versions[doc["uri"]],
            }],
        }})
    elif method == "textDocument/hover":
        pos = msg["params"]["position"]
        send({"jsonrpc": "2.0", "id": mid, "result": {"contents": {"kind": "plaintext",
            "value": "stub hover at %d:%d, configured %s" % (pos["line"], pos["character"], json.dumps(config))}}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "result": None})
"#;

    /// Point the Rust language server at the stub.
    ///
    /// The override is read from the environment when a server is spawned.
    /// The environment belongs to the whole process.
    /// These tests are the only ones that touch it, and they run one after another inside a
    /// single test function.
    fn install_stub(h: &Harness) -> bool {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return false;
        }
        let script = h.path("stub_lsp.py");
        std::fs::write(&script, STUB).unwrap();
        std::env::set_var("LOKODE_LSP_RUST", format!("python3 {}", script.display()));
        true
    }

    /// A symbol is found as a whole word, on the line given or the nearest one that has it.
    /// A column inside a word names that word.
    #[test]
    fn a_symbol_is_found_by_name_near_its_line() {
        let lines = ["let total = sub_total + 1;", "", "fn total() {}"];
        assert_eq!(find_symbol(&lines, 0, "total"), Some((0, 4)));
        assert_eq!(find_symbol(&lines, 1, "total"), Some((0, 4)));
        assert_eq!(find_symbol(&lines, 2, "total"), Some((2, 3)));
        assert_eq!(find_symbol(&lines, 0, "sub"), None);
        assert_eq!(word_at("let total = 1;", 6), "total");
    }

    /// A definition answers with one place, a list of places or links, and all read alike.
    #[test]
    fn places_are_read_from_every_shape_of_answer() {
        let range =
            json!({"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 9}});
        let one = json!({"uri": "file:///p/a.rs", "range": range});
        let link = json!({"targetUri": "file:///p/b.rs", "targetRange": range, "targetSelectionRange": range});
        assert_eq!(locations(&one).len(), 1);
        assert_eq!(locations(&json!([one, link])).len(), 2);
        assert!(locations(&Value::Null).is_empty());
    }

    /// Against a real rust-analyzer: a definition, the uses, an outline, a hover and the
    /// callers of a function, each addressed by its name on a line.
    #[tokio::test]
    #[ignore]
    async fn rust_analyzer_answers_the_navigation_tools() {
        let h = Harness::new(Answer::Yes);
        h.write(
            "Cargo.toml",
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        h.write(
            "src/lib.rs",
            "/// Twice the value.\npub fn twice(x: u32) -> u32 {\n    x * 2\n}\n",
        );
        h.write(
            "src/main.rs",
            "fn main() {\n    let four = probe::twice(2);\n    println!(\"{four}\");\n}\n",
        );
        let pool = LspPool::new(h.root.clone());
        let tool = |op| LspAt {
            pool: pool.clone(),
            op,
        };
        // The first answer can come before the project is indexed.
        // It is asked again until it names the definition.
        let mut definition = String::new();
        for _ in 0..30 {
            definition = tool(Op::Definition)
                .run(
                    &h.ctx,
                    json!({"path": "src/main.rs", "line": 2, "symbol": "twice"}),
                )
                .await
                .unwrap()
                .content;
            if definition.contains("src/lib.rs:2") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        assert!(
            definition.contains("src/lib.rs:2:8  pub fn twice"),
            "{definition}"
        );
        let uses = tool(Op::References)
            .run(
                &h.ctx,
                json!({"path": "src/lib.rs", "line": 2, "symbol": "twice"}),
            )
            .await
            .unwrap();
        assert!(uses.content.contains("src/main.rs:2"), "{}", uses.content);
        let hover = tool(Op::Hover)
            .run(
                &h.ctx,
                json!({"path": "src/main.rs", "line": 1, "symbol": "twice"}),
            )
            .await
            .unwrap();
        assert!(
            hover.content.contains("`twice` is on line 2"),
            "{}",
            hover.content
        );
        assert!(
            hover.content.contains("Twice the value"),
            "{}",
            hover.content
        );
        let callers = tool(Op::Calls)
            .run(
                &h.ctx,
                json!({"path": "src/lib.rs", "line": 2, "symbol": "twice"}),
            )
            .await
            .unwrap();
        assert!(
            callers.content.contains("main  src/main.rs:1"),
            "{}",
            callers.content
        );
        let outline = LspSymbols { pool: pool.clone() }
            .run(&h.ctx, json!({"path": "src/lib.rs"}))
            .await
            .unwrap();
        assert!(
            outline.content.contains("2  fn twice"),
            "{}",
            outline.content
        );
        pool.shutdown().await;
    }

    /// One function, because they share the process environment and a language-server pool.
    #[tokio::test]
    async fn the_language_server_tools_talk_to_a_server() {
        let h = Harness::new(Answer::Yes);
        if !install_stub(&h) {
            eprintln!("skipping: python3 is not available");
            return;
        }
        h.write("a.rs", "fn main() {}\n");
        let pool = LspPool::new(h.root.clone());

        // Diagnostics come back from a server that was actually spoken to.
        let out = LspDiagnostics { pool: pool.clone() }
            .run(&h.ctx, json!({"path": "a.rs"}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("stub saw version"), "{}", out.content);

        // The file is edited behind the server's back, as the agent's own write tools do it.
        // Asking again must describe the new text.
        let first = out.content.clone();
        h.write("a.rs", "fn main() { let x = 1; }\n");
        let after = LspDiagnostics { pool: pool.clone() }
            .run(&h.ctx, json!({"path": "a.rs"}))
            .await
            .unwrap();
        assert_ne!(
            first, after.content,
            "the server was not told the file had changed"
        );
        assert!(after.content.contains("version 2"), "{}", after.content);

        // Hover reaches the same server, which had its own request answered.
        // The column is sent in UTF-16 units: the emoji counts 2.
        h.write("a.rs", "// 🙂 x\n");
        let hover = LspAt {
            pool: pool.clone(),
            op: Op::Hover,
        }
        .run(&h.ctx, json!({"path": "a.rs", "line": 1, "column": 6}))
        .await
        .unwrap();
        assert!(!hover.is_error, "{}", hover.content);
        assert!(
            hover
                .content
                .contains("stub hover at 0:6, configured [null]"),
            "{}",
            hover.content
        );

        // A server that stops is started again once.
        // The second stop leaves it down with what it said.
        h.write("a.rs", "CRASH\n");
        let crashed = LspDiagnostics { pool: pool.clone() }
            .run(&h.ctx, json!({"path": "a.rs"}))
            .await
            .unwrap();
        assert!(crashed.is_error, "{}", crashed.content);
        assert!(crashed.content.contains("stopped"), "{}", crashed.content);
        h.write("a.rs", "fn main() {}\n");
        let again = LspDiagnostics { pool: pool.clone() }
            .run(&h.ctx, json!({"path": "a.rs"}))
            .await
            .unwrap();
        assert!(
            again.content.contains("stub saw version 1"),
            "{}",
            again.content
        );
        h.write("a.rs", "CRASH again\n");
        let _ = LspDiagnostics { pool: pool.clone() }
            .run(&h.ctx, json!({"path": "a.rs"}))
            .await
            .unwrap();
        let down = LspDiagnostics { pool: pool.clone() }
            .run(&h.ctx, json!({"path": "a.rs"}))
            .await
            .unwrap();
        assert!(down.is_error, "{}", down.content);
        assert!(down.content.contains("left down"), "{}", down.content);
        assert!(
            down.content.contains("stub crashed on purpose"),
            "{}",
            down.content
        );

        // A path outside the root is refused before any of this is reached.
        let escaped = LspDiagnostics { pool: pool.clone() }
            .run(&h.ctx, json!({"path": "../outside.rs"}))
            .await
            .unwrap();
        assert!(escaped.is_error, "{}", escaped.content);

        pool.shutdown().await;
        std::env::remove_var("LOKODE_LSP_RUST");
    }
}
