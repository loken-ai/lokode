use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use url::Url;

use crate::sandbox::approval::{Approval, ApprovalRequest, ApprovalScope};
use crate::tools::registry::{Tool, ToolCtx, ToolOutput};

/// How many redirects to follow before giving up.
const MAX_HOPS: usize = 5;

/// Bytes to pull off the wire before abandoning the response.
///
/// The extracted text is clamped to the conversation's budget afterwards, but that
/// happens once the whole body is in memory - so a ceiling that only applies to the text
/// does nothing about a server that offers a hundred gigabytes.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

pub struct WebFetch;

#[derive(Deserialize)]
struct Args {
    url: String,
}

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "GET a URL and return the response body (text). Requires user approval (network egress)."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"url": {"type": "string", "format": "uri"}},
            "required": ["url"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: Args = serde_json::from_value(args)?;
        let mut url = match parse_public(&a.url, &ctx.allow_hosts).await {
            Ok(u) => u,
            Err(e) => return Ok(ToolOutput::err(e)),
        };

        let mut approved: HashSet<String> = HashSet::new();
        let mut status;
        let body;
        let mut hop = 0;

        loop {
            let host = url.host_str().unwrap_or_default().to_string();
            // Asked once per host rather than once per request: a redirect that stays
            // inside the site the user agreed to is the same agreement, and one that
            // leaves it is a different question.
            if approved.insert(host.clone()) {
                match ctx
                    .approve(ApprovalRequest {
                        tool: self.name().into(),
                        summary: if hop == 0 {
                            format!("GET {url}")
                        } else {
                            format!("redirected to {url}")
                        },
                        diff_preview: None,
                        scope: ApprovalScope::Host(host),
                    })
                    .await
                {
                    Approval::Proceed => {}
                    Approval::Denied => return Ok(ToolOutput::err("user denied")),
                    Approval::Interrupted => return Ok(ToolOutput::err("interrupted by user")),
                }
            }

            let resp = tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => return Ok(ToolOutput::err("interrupted by user")),
                r = ctx.http.get(url.clone()).send() => match r {
                    Ok(r) => r,
                    Err(e) => return Ok(ToolOutput::err(format!("request failed: {e}"))),
                },
            };

            status = resp.status();
            if status.is_redirection() {
                // Redirects are followed here rather than by the HTTP client, because the
                // client would follow them without asking where they lead: an approved
                // public URL that answers 302 to 169.254.169.254 or to a service on
                // loopback is the whole shape of the attack, and the address guard has to
                // run again on every hop for it to mean anything.
                hop += 1;
                if hop > MAX_HOPS {
                    return Ok(ToolOutput::err(format!("more than {MAX_HOPS} redirects")));
                }
                let Some(location) = resp.headers().get(reqwest::header::LOCATION) else {
                    return Ok(ToolOutput::err(format!("{status} with no Location header")));
                };
                let Ok(location) = location.to_str() else {
                    return Ok(ToolOutput::err("Location header is not valid text"));
                };
                let Ok(next) = url.join(location) else {
                    return Ok(ToolOutput::err(format!(
                        "unusable redirect target: {location}"
                    )));
                };
                url = match check_public(next, &ctx.allow_hosts).await {
                    Ok(u) => u,
                    Err(e) => return Ok(ToolOutput::err(format!("refused redirect: {e}"))),
                };
                continue;
            }

            body = match read_capped(resp, &ctx.cancel).await {
                Ok(b) => b,
                Err(e) => return Ok(ToolOutput::err(e)),
            };
            break;
        }

        let text = ctx.clamp(strip_html(&body));
        Ok(ToolOutput::ok(format!("status: {status}\n\n{text}")))
    }
}

/// Read the body, giving up once it exceeds what any answer could use.
///
/// Streamed rather than taken whole: `text()` materialises everything the server chooses
/// to send before anyone gets to have an opinion about its size.
async fn read_capped(
    resp: reqwest::Response,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String, String> {
    if let Some(len) = resp.content_length() {
        if len as usize > MAX_BODY_BYTES {
            return Err(format!(
                "response is {len} bytes, over the {MAX_BODY_BYTES} byte ceiling"
            ));
        }
    }
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err("interrupted by user".into()),
            c = stream.next() => match c {
                Some(Ok(c)) => c,
                Some(Err(e)) => return Err(format!("read body failed: {e}")),
                None => break,
            },
        };
        buf.extend_from_slice(&chunk);
        if buf.len() > MAX_BODY_BYTES {
            buf.truncate(MAX_BODY_BYTES);
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Parse a model-supplied string and establish that it points somewhere public.
async fn parse_public(raw: &str, allow: &HashSet<String>) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|e| format!("not a usable URL: {e}"))?;
    check_public(url, allow).await
}

/// Refuse a URL that does not name a public http(s) endpoint.
///
/// The host is resolved here rather than trusted as written, because a name is free to
/// point at a loopback or metadata address and the prefix `https://` says nothing about
/// where the packets go.
async fn check_public(url: Url, allow: &HashSet<String>) -> Result<Url, String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "only http(s) URLs are allowed, got {}",
            url.scheme()
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?
        .to_string();

    if allow.contains(&host) {
        return Ok(url);
    }

    let port = url.port_or_known_default().unwrap_or(80);
    let addrs = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| format!("cannot resolve {host}: {e}"))?;

    let mut saw_none = true;
    for addr in addrs {
        saw_none = false;
        if is_blocked(addr.ip()) {
            return Err(format!(
                "{host} resolves to {}, which is not a public address \
                 (pass --allow-host {host} if that is deliberate)",
                addr.ip()
            ));
        }
    }
    if saw_none {
        return Err(format!("{host} resolves to nothing"));
    }
    Ok(url)
}

/// Whether an address belongs to a range this tool must not reach.
///
/// Loopback and the link-local block that carries cloud metadata are the ones that matter
/// most, but every non-public range is refused: reaching a service inside the network the
/// agent happens to be running in is the same class of mistake.
fn is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

fn is_blocked_v4(v4: Ipv4Addr) -> bool {
    let [a, b, ..] = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_documentation()
        || v4.is_unspecified()
        || v4.is_multicast()
        || a == 0
        // Carrier-grade NAT, 100.64.0.0/10.
        || (a == 100 && (b & 0xc0) == 64)
        // Reserved, 240.0.0.0/4.
        || a >= 240
}

fn is_blocked_v6(v6: Ipv6Addr) -> bool {
    if let Some(mapped) = v6.to_ipv4_mapped() {
        return is_blocked_v4(mapped);
    }
    let first = v6.segments()[0];
    v6.is_loopback()
        || v6.is_unspecified()
        || v6.is_multicast()
        // Unique local, fc00::/7.
        || (first & 0xfe00) == 0xfc00
        // Link-local unicast, fe80::/10.
        || (first & 0xffc0) == 0xfe80
}

/// Reduce a page to the text a model can read.
///
/// Not a DOM parser, and not trying to be. What it does do is drop the contents of `script`
/// and `style` rather than only their tags - a page's JavaScript is many times the size of
/// its prose and none of it is what was asked for - and turn the handful of entities that
/// otherwise arrive as `&amp;` in the middle of a sentence.
fn strip_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    let mut rest = input;

    while !rest.is_empty() {
        // Skip an element whose contents are code rather than text.
        if let Some(end) = skip_element(rest, "script").or_else(|| skip_element(rest, "style")) {
            rest = &rest[end..];
            continue;
        }
        let mut chars = rest.char_indices();
        let Some((_, c)) = chars.next() else { break };
        match c {
            '<' => in_tag = true,
            // A tag is a boundary between words. Without this, `<h1>Title</h1><p>Some` came
            // out as `TitleSome` - the collapse below turns the run of spaces back into one.
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
        rest = &rest[c.len_utf8()..];
    }
    let out = decode_entities(&out);
    // Collapse runs of whitespace
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_space = false;
    for c in out.chars() {
        if c.is_whitespace() {
            if !prev_space {
                collapsed.push(' ');
                prev_space = true;
            }
        } else {
            collapsed.push(c);
            prev_space = false;
        }
    }
    // The tags at each end of a document leave a space behind them.
    collapsed.trim().to_string()
}

/// How far to skip if `rest` opens `tag`, or `None` if it does not.
fn skip_element(rest: &str, tag: &str) -> Option<usize> {
    let open = format!("<{tag}");
    if !rest.len().ge(&open.len()) || !rest[..open.len()].eq_ignore_ascii_case(&open) {
        return None;
    }
    // The character after the name must end it, or `<scripts>` would match `<script`.
    let after = rest[open.len()..].chars().next();
    if !matches!(after, Some(c) if c == '>' || c.is_whitespace()) {
        return None;
    }
    let close = format!("</{tag}");
    let lowered = rest.to_ascii_lowercase();
    match lowered[open.len()..].find(&close) {
        // Past the closing tag's own `>`, or to the end of it if there is none.
        Some(at) => {
            let from = open.len() + at;
            Some(
                lowered[from..]
                    .find('>')
                    .map(|g| from + g + 1)
                    .unwrap_or(rest.len()),
            )
        }
        // An unclosed script runs to the end of the document, and so does this.
        None => Some(rest.len()),
    }
}

/// Turn the entities that actually show up in prose.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        // Last, or it would turn the ampersand of another entity before that entity is read.
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// The address a compromised fetch is aimed at first.
    #[test]
    fn cloud_metadata_is_blocked() {
        assert!(is_blocked(v4("169.254.169.254")));
    }

    #[test]
    fn loopback_and_private_ranges_are_blocked() {
        for addr in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "172.16.0.1",
            "0.0.0.0",
        ] {
            assert!(is_blocked(v4(addr)), "{addr} should be refused");
        }
    }

    #[test]
    fn carrier_grade_nat_is_blocked() {
        assert!(is_blocked(v4("100.64.0.1")));
        assert!(is_blocked(v4("100.127.255.255")));
        // 100.63 and 100.128 are outside the /10 and are ordinary public space.
        assert!(!is_blocked(v4("100.63.255.255")));
        assert!(!is_blocked(v4("100.128.0.1")));
    }

    #[test]
    fn public_addresses_are_allowed() {
        for addr in ["1.1.1.1", "8.8.8.8", "93.184.216.34"] {
            assert!(!is_blocked(v4(addr)), "{addr} should be allowed");
        }
    }

    /// An IPv4 address wearing an IPv6 costume is the same address.
    #[test]
    fn ipv4_mapped_addresses_are_judged_as_ipv4() {
        assert!(is_blocked(v4("::ffff:127.0.0.1")));
        assert!(is_blocked(v4("::ffff:169.254.169.254")));
        assert!(!is_blocked(v4("::ffff:8.8.8.8")));
    }

    #[test]
    fn ipv6_local_ranges_are_blocked() {
        for addr in ["::1", "::", "fc00::1", "fd12:3456::1", "fe80::1"] {
            assert!(is_blocked(v4(addr)), "{addr} should be refused");
        }
        assert!(!is_blocked(v4("2606:4700:4700::1111")));
    }

    #[tokio::test]
    async fn non_http_schemes_are_refused() {
        let allow = HashSet::new();
        for raw in ["file:///etc/passwd", "ftp://example.com/x", "gopher://x/"] {
            assert!(
                parse_public(raw, &allow).await.is_err(),
                "{raw} should be refused"
            );
        }
    }

    /// A literal address needs no DNS, so this exercises the guard without a network.
    #[tokio::test]
    async fn a_literal_loopback_url_is_refused() {
        let allow = HashSet::new();
        let err = parse_public("http://127.0.0.1:8080/admin", &allow)
            .await
            .unwrap_err();
        assert!(
            err.contains("not a public address"),
            "unexpected message: {err}"
        );
    }

    /// The escape hatch has to actually open, or anyone with a LAN service is stuck.
    #[tokio::test]
    async fn an_allowed_host_bypasses_the_guard() {
        let allow: HashSet<String> = ["127.0.0.1".to_string()].into_iter().collect();
        assert!(parse_public("http://127.0.0.1:8080/admin", &allow)
            .await
            .is_ok());
    }

    /// Serve one response and hang up. Enough to be redirected by.
    async fn serve_once(response: &'static str) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut scratch = [0u8; 1024];
                let _ = sock.read(&mut scratch).await;
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        port
    }

    fn test_ctx(allow: &[&str]) -> ToolCtx {
        use crate::sandbox::approval::{self, ApprovalDecision, SessionApprovals};
        use std::sync::Arc;

        let (tx, mut rx) = approval::channel();
        tokio::spawn(async move {
            while let Some(p) = rx.recv().await {
                let _ = p.reply.send(ApprovalDecision::Once);
            }
        });
        ToolCtx {
            policy: Arc::new(Default::default()),
            auto_used: Arc::new(Default::default()),
            honour_allow: true,
            live: None,
            root: std::env::temp_dir(),
            approval: tx,
            session_approvals: Arc::new(SessionApprovals::new()),
            max_output_chars: 4096,
            cancel: tokio_util::sync::CancellationToken::new(),
            http: crate::tools::registry::tool_http_client().unwrap(),
            allow_hosts: Arc::new(allow.iter().map(|s| s.to_string()).collect()),
            shell_writable: None,
            known: Default::default(),
        }
    }

    /// The attack this guard exists for: a host the user agreed to answers with a
    /// redirect onto the cloud metadata address. Checking only the URL the user was shown
    /// would let it through, so the check has to run again on the hop.
    #[tokio::test]
    async fn a_redirect_towards_metadata_is_refused_at_the_hop() {
        let port = serve_once(
            "HTTP/1.1 302 Found\r\n\
             Location: http://169.254.169.254/latest/meta-data/\r\n\
             Content-Length: 0\r\n\r\n",
        )
        .await;

        // The entry point is allowed explicitly, so the only thing that can refuse the
        // request is the guard running on the redirect target.
        let ctx = test_ctx(&["127.0.0.1"]);
        let out = WebFetch
            .run(&ctx, json!({ "url": format!("http://127.0.0.1:{port}/") }))
            .await
            .unwrap();

        assert!(
            out.is_error,
            "the redirect should have been refused: {}",
            out.content
        );
        assert!(
            out.content.contains("refused redirect") && out.content.contains("169.254.169.254"),
            "unexpected message: {}",
            out.content
        );
    }

    /// A response that is not a redirect still has to come back intact.
    #[tokio::test]
    async fn an_ordinary_response_is_read() {
        let port = serve_once(
            "HTTP/1.1 200 OK\r\n\
             Content-Length: 26\r\n\r\n\
             <html><b>hello</b></html>\n",
        )
        .await;

        let ctx = test_ctx(&["127.0.0.1"]);
        let out = WebFetch
            .run(&ctx, json!({ "url": format!("http://127.0.0.1:{port}/") }))
            .await
            .unwrap();

        assert!(!out.is_error, "unexpected failure: {}", out.content);
        assert!(
            out.content.contains("hello"),
            "body missing: {}",
            out.content
        );
        assert!(
            !out.content.contains("<b>"),
            "tags should be stripped: {}",
            out.content
        );
    }
}

#[cfg(test)]
mod html_tests {
    use super::*;

    /// A page's JavaScript is many times the size of its prose and none of it is what was
    /// asked for. Stripping only the tags left every line of it in the answer.
    #[test]
    fn a_script_body_is_dropped_not_just_its_tags() {
        let html = "<p>before</p><script>var x = 1; alert('noise');</script><p>after</p>";
        let text = strip_html(html);
        assert!(text.contains("before"), "{text}");
        assert!(text.contains("after"), "{text}");
        assert!(!text.contains("alert"), "the script body survived: {text}");
    }

    #[test]
    fn a_style_body_is_dropped_too() {
        let text = strip_html("<style>.a{color:red}</style>hello");
        assert!(text.contains("hello"), "{text}");
        assert!(!text.contains("color"), "{text}");
    }

    /// An element whose name merely starts with `script` is a different element.
    #[test]
    fn a_similarly_named_element_is_not_mistaken_for_one() {
        let text = strip_html("<scripting>kept</scripting>");
        assert!(text.contains("kept"), "{text}");
    }

    /// An unclosed script runs to the end of the document, and so must the skip - otherwise
    /// the rest of the page is code presented as prose.
    #[test]
    fn an_unclosed_script_swallows_the_rest() {
        let text = strip_html("<p>before</p><script>var x = 1;");
        assert!(text.contains("before"), "{text}");
        assert!(!text.contains("var x"), "{text}");
    }

    #[test]
    fn the_entities_that_appear_in_prose_are_turned() {
        assert_eq!(strip_html("a &amp; b"), "a & b");
        assert_eq!(strip_html("&lt;tag&gt;"), "<tag>");
        assert_eq!(strip_html("it&#39;s"), "it's");
    }

    /// `&amp;lt;` is an escaped `&lt;`, not a `<`. Turning `&amp;` first would make it one.
    #[test]
    fn a_doubly_escaped_entity_is_only_decoded_once() {
        assert_eq!(strip_html("&amp;lt;"), "&lt;");
    }

    #[test]
    fn ordinary_text_survives_unchanged() {
        assert_eq!(
            strip_html("<h1>Title</h1><p>Some words.</p>"),
            "Title Some words."
        );
    }
}
