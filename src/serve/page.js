// The page of `lokode serve`: an Agent Client Protocol client, as an editor is one.
//
// What lokode sends arrives on /events; what the page sends is posted to /rpc. A request
// of the page's is answered on the stream, matched by an id this page alone uses.
"use strict";

const $ = (id) => document.getElementById(id);
const tab = Math.random().toString(36).slice(2, 10);
let counter = 0;
const waiting = new Map();
const state = {
  root: "",
  session: null,
  running: false,
  commands: [],
  changes: new Set(),
  steps: new Map(),
  answer: null,
  answerText: "",
  thought: null,
  cards: new Map(),
};

// ------------------------------------------------------------------ transport --

async function post(msg) {
  const res = await fetch("/rpc", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(msg),
  });
  return res.json();
}

/// A request of lokode's own, answered in the response.
async function own(method, params) {
  const got = await post({ jsonrpc: "2.0", id: `${tab}-${++counter}`, method, params });
  return got.result;
}

/// A protocol request, answered on the stream.
function request(method, params) {
  const id = `${tab}-${++counter}`;
  return new Promise((resolve, reject) => {
    waiting.set(id, { resolve, reject });
    post({ jsonrpc: "2.0", id, method, params }).catch(reject);
  });
}

function notify(method, params) {
  post({ jsonrpc: "2.0", method, params });
}

function answer(id, result) {
  post({ jsonrpc: "2.0", id, result });
}

function listen() {
  const events = new EventSource("/events");
  let lost = false;
  events.onopen = () => {
    $("link").textContent = "connected";
    // What was said while the stream was down is taken again from the server.
    if (lost && state.session) attach(state.session);
    lost = false;
  };
  events.onerror = () => { $("link").textContent = "reconnecting"; lost = true; };
  events.addEventListener("lagged", () => { if (state.session) attach(state.session); });
  events.onmessage = (e) => {
    const msg = JSON.parse(e.data);
    if (msg.id !== undefined && msg.method === undefined) {
      const w = waiting.get(msg.id);
      if (!w) return;
      waiting.delete(msg.id);
      msg.error ? w.reject(new Error(msg.error.message)) : w.resolve(msg.result);
    } else if (msg.method === "session/update") {
      if (msg.params.sessionId === state.session) render(msg.params.update);
    } else if (msg.method === "_lokode/settled") {
      const card = state.cards.get(msg.params.id);
      if (card) { card.remove(); state.cards.delete(msg.params.id); }
    } else if (msg.id !== undefined) {
      if (msg.params && msg.params.sessionId === state.session) asked(msg);
    }
  };
}

// ------------------------------------------------------------------ rendering --

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

function append(node) {
  const thread = $("thread");
  const atEnd = thread.scrollHeight - thread.scrollTop - thread.clientHeight < 80;
  thread.appendChild(node);
  if (atEnd) thread.scrollTop = thread.scrollHeight;
}

function settleAnswer() {
  state.answer = null;
  state.answerText = "";
  state.thought = null;
}

function render(u) {
  switch (u.sessionUpdate) {
    case "user_message_chunk": {
      settleAnswer();
      append(el("div", "user", u.content.text));
      break;
    }
    case "agent_message_chunk": {
      state.thought = null;
      if (!state.answer) {
        state.answer = el("div", "answer");
        append(state.answer);
      }
      state.answerText += u.content.text || "";
      state.answer.innerHTML = markdown(state.answerText);
      break;
    }
    case "agent_thought_chunk": {
      if (!state.thought) {
        state.thought = el("div", "thought");
        append(state.thought);
      }
      state.thought.textContent += u.content.text || "";
      break;
    }
    case "tool_call": {
      settleAnswer();
      const step = el("details", "step running");
      const head = el("summary");
      head.append(el("span", "dot", "◐"), el("span", "title", u.title), el("span", "kind", u.kind || ""));
      step.appendChild(head);
      step.appendChild(el("div", "out"));
      state.steps.set(u.toolCallId, step);
      append(step);
      break;
    }
    case "tool_call_update": {
      const step = state.steps.get(u.toolCallId);
      if (!step) break;
      if (u.status) {
        step.className = `step ${u.status === "in_progress" ? "running" : u.status}`;
        step.querySelector(".dot").textContent =
          u.status === "completed" ? "●" : u.status === "failed" ? "✕" : "◐";
      }
      const out = step.querySelector(".out");
      for (const c of u.content || []) {
        if (c.type === "diff") {
          out.appendChild(diffView(c.path, c.oldText, c.newText));
          step.open = true;
        } else if (c.type === "content" && c.content && c.content.text) {
          out.appendChild(unified(c.content.text));
        }
      }
      break;
    }
    case "plan": {
      const list = $("plan");
      list.replaceChildren();
      for (const e of u.entries) {
        const mark = e.status === "completed" ? "✓" : e.status === "in_progress" ? "◐" : "○";
        list.appendChild(el("li", e.status, `${mark} ${e.content}`));
      }
      if (!u.entries.length) list.appendChild(el("li", "dim", "no task list yet"));
      break;
    }
    case "current_mode_update": {
      $("mode").value = u.currentModeId;
      break;
    }
    case "available_commands_update": {
      state.commands = u.availableCommands || [];
      break;
    }
    case "_lokode/turn": {
      settleAnswer();
      if (u.stopReason === "cancelled") append(el("div", "receipt", "stopped"));
      showMeta(u._meta);
      break;
    }
  }
}

/// A question of lokode's: an approval or a tool server's form.
function asked(msg) {
  if (state.cards.has(msg.id)) return;
  const card = el("div", "card");
  if (msg.method === "session/request_permission") {
    const p = msg.params;
    card.appendChild(el("div", "q", `◉ ${p.toolCall.title || "a change"} waits for you`));
    for (const c of p.toolCall.content || []) {
      if (c.content && c.content.text) card.appendChild(unified(c.content.text));
    }
    const row = el("div", "options");
    for (const o of p.options) {
      const b = el("button", o.kind === "allow_once" ? "primary" : "", o.name);
      b.type = "button";
      b.onclick = () => {
        answer(msg.id, { outcome: { outcome: "selected", optionId: o.optionId } });
        card.remove();
        state.cards.delete(msg.id);
      };
      row.appendChild(b);
    }
    card.appendChild(row);
  } else if (msg.method === "elicitation/create") {
    card.appendChild(form(msg));
  } else {
    answer(msg.id, null);
    return;
  }
  state.cards.set(msg.id, card);
  append(card);
}

function form(msg) {
  const p = msg.params;
  const f = el("form");
  f.appendChild(el("div", "q", p.message));
  const props = (p.requestedSchema && p.requestedSchema.properties) || {};
  const required = (p.requestedSchema && p.requestedSchema.required) || [];
  for (const [name, s] of Object.entries(props)) {
    const label = el("label", "", `${s.title || name}${required.includes(name) ? "" : " (may be left empty)"}`);
    let input;
    if (s.enum) {
      input = el("select");
      if (!required.includes(name)) input.appendChild(el("option", "", ""));
      for (const v of s.enum) input.appendChild(el("option", "", v));
    } else if (s.type === "boolean") {
      input = el("input");
      input.type = "checkbox";
      input.checked = !!s.default;
    } else {
      input = el("input");
      input.type = s.type === "number" || s.type === "integer" ? "number" : "text";
      if (s.type === "integer") input.step = "1";
      if (s.default !== undefined) input.value = s.default;
      input.required = required.includes(name);
    }
    input.name = name;
    label.appendChild(input);
    if (s.description) label.appendChild(el("span", "dim", s.description));
    f.appendChild(label);
  }
  const row = el("div", "options");
  const ok = el("button", "primary", "Answer");
  ok.type = "submit";
  const no = el("button", "", "Decline");
  no.type = "button";
  row.append(ok, no);
  f.appendChild(row);
  const done = () => { f.parentElement && f.parentElement.remove(); state.cards.delete(msg.id); };
  f.onsubmit = (e) => {
    e.preventDefault();
    const content = {};
    for (const [name, s] of Object.entries(props)) {
      const input = f.elements[name];
      if (s.type === "boolean") content[name] = input.checked;
      else if (input.value === "") continue;
      else if (s.type === "number") content[name] = Number(input.value);
      else if (s.type === "integer") content[name] = parseInt(input.value, 10);
      else content[name] = input.value;
    }
    answer(msg.id, { action: "accept", content });
    done();
  };
  no.onclick = () => { answer(msg.id, { action: "decline" }); done(); };
  return f;
}

// ------------------------------------------------------------------ diff --

/// A unified diff, fenced or not, with its added and removed lines marked. Other text as it is.
function unified(text) {
  const fenced = text.replace(/^```diff\n/, "").replace(/\n?```\s*$/, "");
  const body = fenced !== text ? fenced : text;
  if (fenced === text && !(/^--- /m.test(text) && /^\+\+\+ /m.test(text))) return el("pre", "", text);
  const box = el("div", "diff");
  for (const line of body.split("\n")) {
    const cls = line.startsWith("+") && !line.startsWith("+++") ? "l add"
      : line.startsWith("-") && !line.startsWith("---") ? "l del"
      : line.startsWith("@@") ? "l gap" : "l";
    box.appendChild(el("div", cls, line));
  }
  return box;
}

/// The lines that changed between two texts, with three lines around each change.
function diffView(path, oldText, newText) {
  const box = el("div", "diff");
  const shown = path.startsWith(state.root + "/") ? path.slice(state.root.length + 1) : path;
  box.appendChild(el("div", "file", shown));
  const lines = (t) => (t.endsWith("\n") ? t.slice(0, -1) : t).split("\n");
  const a = oldText === null || oldText === undefined ? [] : lines(oldText);
  const b = lines(newText);
  const ops = lineDiff(a, b);
  if (!ops) {
    box.appendChild(el("div", "l gap", `rewritten, ${b.length} lines`));
    return box;
  }
  const keep = new Array(ops.length).fill(false);
  ops.forEach((op, i) => {
    if (op[0] !== " ") for (let j = Math.max(0, i - 3); j <= Math.min(ops.length - 1, i + 3); j++) keep[j] = true;
  });
  let skipped = false;
  ops.forEach((op, i) => {
    if (!keep[i]) { skipped = true; return; }
    if (skipped) { box.appendChild(el("div", "l gap", "…")); skipped = false; }
    const cls = op[0] === "+" ? "l add" : op[0] === "-" ? "l del" : "l";
    box.appendChild(el("div", cls, `${op[0]} ${op[1]}`));
  });
  return box;
}

/// The longest common subsequence of lines, or null when the texts are too long for it.
function lineDiff(a, b) {
  if (a.length * b.length > 4000000) return null;
  const n = a.length, m = b.length;
  const dp = Array.from({ length: n + 1 }, () => new Uint32Array(m + 1));
  for (let i = n - 1; i >= 0; i--)
    for (let j = m - 1; j >= 0; j--)
      dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
  const ops = [];
  let i = 0, j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) { ops.push([" ", a[i]]); i++; j++; }
    else if (dp[i + 1][j] >= dp[i][j + 1]) ops.push(["-", a[i++]]);
    else ops.push(["+", b[j++]]);
  }
  while (i < n) ops.push(["-", a[i++]]);
  while (j < m) ops.push(["+", b[j++]]);
  return ops;
}

// ------------------------------------------------------------------ markdown --

function escape(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

function inline(s) {
  return escape(s)
    .replace(/`([^`]+)`/g, "<code>$1</code>")
    .replace(/\*\*([^*]+)\*\*/g, "<b>$1</b>")
    .replace(/(^|[^*])\*([^*\s][^*]*)\*/g, "$1<em>$2</em>")
    .replace(/\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g, '<a href="$2" rel="noreferrer noopener" target="_blank">$1</a>');
}

/// What a model writes, as HTML: headings, lists, tables, code, emphasis and links.
function markdown(text) {
  const out = [];
  const lines = text.split("\n");
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (line.startsWith("```")) {
      const body = [];
      i++;
      while (i < lines.length && !lines[i].startsWith("```")) body.push(lines[i++]);
      i++;
      out.push(`<pre><code>${escape(body.join("\n"))}</code></pre>`);
      continue;
    }
    const h = line.match(/^(#{1,4})\s+(.*)$/);
    if (h) { out.push(`<h${h[1].length + 2}>${inline(h[2])}</h${h[1].length + 2}>`); i++; continue; }
    if (/^\s*\|.*\|\s*$/.test(line) && i + 1 < lines.length && /^\s*\|[\s:|-]+\|\s*$/.test(lines[i + 1])) {
      const cells = (l) => l.trim().replace(/^\||\|$/g, "").split("|").map((c) => inline(c.trim()));
      const head = cells(line);
      i += 2;
      const rows = [];
      while (i < lines.length && /^\s*\|.*\|\s*$/.test(lines[i])) rows.push(cells(lines[i++]));
      out.push(`<table><tr>${head.map((c) => `<th>${c}</th>`).join("")}</tr>${rows
        .map((r) => `<tr>${r.map((c) => `<td>${c}</td>`).join("")}</tr>`).join("")}</table>`);
      continue;
    }
    if (/^\s*([-*]|\d+\.)\s+/.test(line)) {
      const ordered = /^\s*\d+\./.test(line);
      const items = [];
      while (i < lines.length && /^\s*([-*]|\d+\.)\s+/.test(lines[i])) items.push(inline(lines[i++].replace(/^\s*([-*]|\d+\.)\s+/, "")));
      const tag = ordered ? "ol" : "ul";
      out.push(`<${tag}>${items.map((t) => `<li>${t}</li>`).join("")}</${tag}>`);
      continue;
    }
    if (!line.trim()) { i++; continue; }
    const para = [];
    while (i < lines.length && lines[i].trim() && !lines[i].startsWith("```") && !/^(#{1,4})\s/.test(lines[i]) && !/^\s*([-*]|\d+\.)\s+/.test(lines[i]) && !/^\s*\|.*\|\s*$/.test(lines[i])) para.push(lines[i++]);
    out.push(`<p>${inline(para.join("\n")).replace(/\n/g, "<br>")}</p>`);
  }
  return out.join("");
}

// ------------------------------------------------------------------ the rail --

function showMeta(meta) {
  const l = meta && meta.lokode;
  if (!l) return;
  if (l.turn) {
    for (const f of l.turn.files || []) state.changes.add(f);
    const list = $("changes");
    list.replaceChildren();
    for (const f of state.changes) list.appendChild(el("li", "", f));
    if (!state.changes.size) list.appendChild(el("li", "dim", "nothing changed yet"));
    const t = l.turn;
    const parts = [`${(t.tookMs / 1000).toFixed(1)} s`, `${t.steps} step(s)`];
    if (t.tokensIn + t.tokensOut) parts.push(`${t.tokensIn} in, ${t.tokensOut} out`);
    if (t.files.length) parts.push(`${t.files.length} file(s) changed`);
    if (t.joules) parts.push(`${(t.joules / 3600).toFixed(2)} Wh`);
    append(el("div", "receipt", `✓ ${parts.join(" · ")}`));
  }
  if (l.window) {
    $("window-bar").style.width = `${l.window.percent}%`;
    $("window").textContent = `${l.window.percent}% of ${l.window.budget} characters`;
  }
  const engine = [];
  if (l.tokensPerSecond) engine.push(`${l.tokensPerSecond.toFixed(0)} tokens/s`);
  if (l.energy && l.energy.turnJoules) engine.push(`turn ${(l.energy.turnJoules / 3600).toFixed(2)} Wh`);
  if (l.energy && l.energy.sessionJoules) engine.push(`session ${(l.energy.sessionJoules / 3600).toFixed(2)} Wh`);
  if (engine.length) $("engine").textContent = engine.join(" · ");
}

async function sessions(hello) {
  const list = $("sessions");
  list.replaceChildren();
  const seen = new Set();
  for (const id of hello.open) {
    seen.add(id);
    const li = el("li", id === state.session ? "on" : "", `● ${id.slice(0, 8)} open`);
    li.onclick = () => attach(id);
    list.appendChild(li);
  }
  for (const r of hello.recorded) {
    if (seen.has(r.id)) continue;
    const li = el("li", "", `○ ${r.opening || r.id.slice(0, 8)}`);
    li.title = `${r.messages} message(s)`;
    li.onclick = () => load(r.id);
    list.appendChild(li);
  }
}

function modes(m) {
  const select = $("mode");
  select.replaceChildren();
  for (const mode of m.availableModes) {
    const o = el("option", "", mode.name);
    o.value = mode.id;
    o.title = mode.description || "";
    select.appendChild(o);
  }
  select.value = m.currentModeId;
}

// ------------------------------------------------------------------ sessions --

function clear(id) {
  state.session = id;
  state.steps.clear();
  state.cards.clear();
  state.changes.clear();
  settleAnswer();
  $("thread").replaceChildren();
  $("session").textContent = id.slice(0, 8);
}

async function attach(id) {
  clear(id);
  const got = await own("_lokode/attach", { sessionId: id });
  for (const u of got.updates) render(u);
  for (const a of got.asks) asked(a);
  const r = await request("session/load", { sessionId: id, cwd: state.root, mcpServers: [] });
  if (r && r.modes) modes(r.modes);
  sessions(await own("_lokode/hello", {}));
}

async function load(id) {
  clear(id);
  const r = await request("session/load", { sessionId: id, cwd: state.root, mcpServers: [] });
  if (r && r.modes) modes(r.modes);
  sessions(await own("_lokode/hello", {}));
}

async function fresh() {
  const r = await request("session/new", { cwd: state.root, mcpServers: [] });
  clear(r.sessionId);
  modes(r.modes);
  sessions(await own("_lokode/hello", {}));
}

// ------------------------------------------------------------------ asking --

function running(on) {
  state.running = on;
  $("stop").hidden = !on;
  $("send").disabled = on;
  $("status").textContent = on ? "working" : "";
}

async function send(text) {
  if (!text.trim() || !state.session || state.running) return;
  running(true);
  settleAnswer();
  try {
    await request("session/prompt", { sessionId: state.session, prompt: [{ type: "text", text }] });
  } catch (e) {
    append(el("div", "thought", `failed: ${e.message}`));
  } finally {
    running(false);
    settleAnswer();
  }
}

function completions() {
  const box = $("commands");
  const text = $("input").value;
  const m = text.match(/^\/(\S*)$/);
  if (!m) { box.hidden = true; return; }
  const shown = state.commands.filter((c) => c.name.startsWith(m[1]));
  box.replaceChildren();
  shown.forEach((c, i) => {
    const row = el("div", i === 0 ? "on" : "");
    row.append(el("b", "", `/${c.name}`), el("span", "dim", c.description || ""));
    row.onclick = () => { $("input").value = `/${c.name} `; box.hidden = true; $("input").focus(); };
    box.appendChild(row);
  });
  box.hidden = shown.length === 0;
}

$("ask").onsubmit = (e) => {
  e.preventDefault();
  const text = $("input").value;
  $("input").value = "";
  $("commands").hidden = true;
  send(text);
};
$("input").addEventListener("keydown", (e) => {
  const box = $("commands");
  if (!box.hidden && (e.key === "Tab" || (e.key === "Enter" && /^\/\S*$/.test($("input").value)))) {
    const on = box.querySelector(".on");
    if (on) { e.preventDefault(); on.click(); return; }
  }
  if (e.key === "Escape" && !box.hidden) { box.hidden = true; return; }
  if (e.key === "Escape" && state.running) { notify("session/cancel", { sessionId: state.session }); return; }
  if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); $("ask").requestSubmit(); }
});
$("input").addEventListener("input", completions);
$("stop").onclick = () => notify("session/cancel", { sessionId: state.session });
$("mode").onchange = () => request("session/set_mode", { sessionId: state.session, modeId: $("mode").value });
$("new-session").onclick = fresh;

// ------------------------------------------------------------------ start --

(async () => {
  listen();
  const hello = await own("_lokode/hello", {});
  state.root = hello.root;
  $("project").textContent = hello.root.split("/").pop();
  document.title = `lokode · ${$("project").textContent}`;
  await request("initialize", {
    protocolVersion: 1,
    clientCapabilities: { elicitation: { form: {} } },
    clientInfo: { name: "lokode-page", version: hello.version },
  });
  if (hello.open.length) await attach(hello.open[hello.open.length - 1]);
  else await fresh();
  $("input").focus();
})();
