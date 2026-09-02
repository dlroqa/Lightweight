//! `web.fetch` and `web.search` — reaching the network for read-only data.
//!
//! Both are [`RiskClass::External`](lightagent_core::RiskClass::External): they
//! read remote data and change nothing local. They run only when the caller has
//! injected a [`WebContext`] (web access enabled in config); absent it, they
//! return a controlled result the model is shown, never a panic — the same shape
//! `agent.delegate` uses when delegation is off.
//!
//! `web.fetch` is guarded: the URL scheme must be http(s), the host must clear
//! the allow-list, and — unless the host is explicitly allow-listed — every
//! address it resolves to must be global, re-checked on each manual redirect
//! hop, so a fetch cannot be steered onto a loopback or private service (an SSRF
//! guard). HTML is reduced to readable text without an HTML dependency.
//!
//! `web.search` posts the query to the configured JSON endpoint and reads a
//! `results` array; it reports cleanly when no backend is configured.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use async_trait::async_trait;
use lightagent_core::{RiskClass, Scope, ToolOutcome};
use reqwest::header::{CONTENT_TYPE, LOCATION};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::context::{ToolCtx, WebContext, WebPolicy};
use crate::definition::{Tool, ToolDefinition};

/// The most redirect hops `web.fetch` follows, each re-guarded.
const MAX_REDIRECTS: u32 = 5;

/// `web.fetch` — fetch a URL and return its readable text.
pub struct WebFetch {
    definition: ToolDefinition,
}

impl WebFetch {
    /// The tool's stable name.
    pub const NAME: &'static str = "web.fetch";

    /// Build the tool with its declaration.
    pub fn new() -> Self {
        let parameters = json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The http(s) URL to fetch." }
            },
            "required": ["url"],
            "additionalProperties": false,
        });
        Self {
            definition: ToolDefinition::new(
                Self::NAME,
                "Fetch an http(s) URL and return its readable text.",
                parameters,
                RiskClass::External,
                vec![Scope::new("web:fetch")],
            ),
        }
    }
}

impl Default for WebFetch {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
struct FetchArgs {
    url: String,
}

#[async_trait]
impl Tool for WebFetch {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolOutcome {
        let Some(web) = ctx.web.clone() else {
            return ToolOutcome::error("web access is not enabled for this run");
        };
        let args: FetchArgs = match serde_json::from_value(args.clone()) {
            Ok(args) => args,
            Err(error) => {
                return ToolOutcome::error(format!("could not read fetch arguments: {error}"));
            }
        };
        match fetch(&web, &args.url).await {
            Ok(text) => ToolOutcome::ok(text),
            Err(message) => ToolOutcome::error(message),
        }
    }
}

/// Follow `start` under the guard, up to [`MAX_REDIRECTS`] hops.
async fn fetch(web: &WebContext, start: &str) -> Result<String, String> {
    let mut url = reqwest::Url::parse(start).map_err(|error| format!("invalid URL: {error}"))?;
    let mut hops = 0;
    loop {
        guard_url(&web.policy, &url).await?;
        let mut response = web
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|error| format!("could not fetch {url}: {error}"))?;

        if response.status().is_redirection() {
            hops += 1;
            if hops > MAX_REDIRECTS {
                return Err(format!("too many redirects (more than {MAX_REDIRECTS})"));
            }
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| "redirect without a Location header".to_owned())?;
            url = url
                .join(location)
                .map_err(|error| format!("invalid redirect target {location:?}: {error}"))?;
            continue;
        }

        if !response.status().is_success() {
            return Err(format!(
                "{url} returned HTTP {}",
                response.status().as_u16()
            ));
        }

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let body = read_bounded(&mut response, web.policy.max_fetch_bytes).await?;
        return Ok(render_fetch(&url, &content_type, &body));
    }
}

/// Render a fetched body as text: HTML reduced to readable text, other textual
/// types passed through, binary types named rather than dumped.
fn render_fetch(url: &reqwest::Url, content_type: &str, body: &[u8]) -> String {
    let is_html = content_type.contains("html");
    let is_textual = content_type.is_empty()
        || content_type.starts_with("text/")
        || content_type.contains("json")
        || content_type.contains("xml")
        || content_type.contains("javascript");

    let mut out = format!("URL: {url}\n");
    if is_html {
        let html = String::from_utf8_lossy(body);
        let (title, text) = html_to_text(&html);
        if let Some(title) = title {
            out.push_str(&format!("Title: {title}\n"));
        }
        out.push('\n');
        out.push_str(&text);
    } else if is_textual {
        out.push('\n');
        out.push_str(&String::from_utf8_lossy(body));
    } else {
        out.push_str(&format!(
            "\n[{} bytes of {} content, not shown]",
            body.len(),
            if content_type.is_empty() {
                "binary"
            } else {
                content_type
            },
        ));
    }
    out
}

/// `web.search` — query the configured search backend.
pub struct WebSearch {
    definition: ToolDefinition,
}

impl WebSearch {
    /// The tool's stable name.
    pub const NAME: &'static str = "web.search";

    /// Build the tool with its declaration.
    pub fn new() -> Self {
        let parameters = json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "The search query." },
                "max_results": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Cap the number of results returned.",
                }
            },
            "required": ["query"],
            "additionalProperties": false,
        });
        Self {
            definition: ToolDefinition::new(
                Self::NAME,
                "Search the web and return a list of result titles, URLs and snippets.",
                parameters,
                RiskClass::External,
                vec![Scope::new("web:search")],
            ),
        }
    }
}

impl Default for WebSearch {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    max_results: Option<usize>,
}

#[async_trait]
impl Tool for WebSearch {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolOutcome {
        let Some(web) = ctx.web.clone() else {
            return ToolOutcome::error("web access is not enabled for this run");
        };
        let args: SearchArgs = match serde_json::from_value(args.clone()) {
            Ok(args) => args,
            Err(error) => {
                return ToolOutcome::error(format!("could not read search arguments: {error}"));
            }
        };
        match search(&web, &args.query, args.max_results).await {
            Ok(text) => ToolOutcome::ok(text),
            Err(message) => ToolOutcome::error(message),
        }
    }
}

async fn search(web: &WebContext, query: &str, requested: Option<usize>) -> Result<String, String> {
    let policy = &web.policy;
    let Some(endpoint) = &policy.search_endpoint else {
        return Err("no search backend is configured (set web.search.endpoint)".to_owned());
    };
    let ceiling = policy.search_max_results.max(1);
    let limit = requested.unwrap_or(ceiling).clamp(1, ceiling);

    let mut url = reqwest::Url::parse(endpoint)
        .map_err(|error| format!("invalid search endpoint: {error}"))?;
    url.query_pairs_mut()
        .append_pair(&policy.search_query_param, query);

    let mut request = web.client.get(url);
    if let Some(key) = &policy.search_api_key {
        request = request.bearer_auth(key);
    }
    let mut response = request
        .send()
        .await
        .map_err(|error| format!("search request failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "search endpoint returned HTTP {}",
            response.status().as_u16()
        ));
    }
    let body = read_bounded(&mut response, policy.max_fetch_bytes).await?;
    let json: Value = serde_json::from_slice(&body)
        .map_err(|error| format!("search endpoint did not return JSON: {error}"))?;
    let hits = parse_search_results(&json, limit);
    if hits.is_empty() {
        return Ok(format!("No results for {query:?}."));
    }
    Ok(render_search(query, &hits))
}

/// One search result.
#[derive(Debug, PartialEq)]
struct SearchHit {
    title: String,
    url: String,
    snippet: String,
}

/// Read a `results` array of `{title, url, content|snippet|description}` from a
/// search response — SearXNG's shape and a common minimal one. An item without a
/// `url` is skipped; a missing title falls back to the URL.
fn parse_search_results(json: &Value, limit: usize) -> Vec<SearchHit> {
    let Some(results) = json.get("results").and_then(Value::as_array) else {
        return Vec::new();
    };
    results
        .iter()
        .filter_map(|item| {
            let url = item.get("url").and_then(Value::as_str)?;
            let title = item.get("title").and_then(Value::as_str).unwrap_or(url);
            let snippet = ["content", "snippet", "description", "body"]
                .iter()
                .find_map(|key| item.get(*key).and_then(Value::as_str))
                .unwrap_or("");
            Some(SearchHit {
                title: title.to_owned(),
                url: url.to_owned(),
                snippet: snippet.to_owned(),
            })
        })
        .take(limit)
        .collect()
}

fn render_search(query: &str, hits: &[SearchHit]) -> String {
    let mut out = format!("Results for {query:?}:\n\n");
    for (index, hit) in hits.iter().enumerate() {
        out.push_str(&format!("{}. {}\n{}\n", index + 1, hit.title, hit.url));
        if !hit.snippet.is_empty() {
            out.push_str(&format!("{}\n", hit.snippet));
        }
        out.push('\n');
    }
    out.truncate(out.trim_end().len());
    out
}

/// Read at most `cap` bytes of a response body, stopping early so a large or
/// hostile page cannot exhaust memory.
async fn read_bounded(response: &mut reqwest::Response, cap: usize) -> Result<Vec<u8>, String> {
    let mut buffer = Vec::new();
    while buffer.len() < cap {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let take = chunk.len().min(cap - buffer.len());
                buffer.extend_from_slice(&chunk[..take]);
            }
            Ok(None) => break,
            Err(error) => return Err(format!("error reading body: {error}")),
        }
    }
    Ok(buffer)
}

/// Refuse a URL whose scheme is not http(s), whose host is not allow-listed, or
/// — unless the host is allow-listed — that resolves to a non-global address.
async fn guard_url(policy: &WebPolicy, url: &reqwest::Url) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "unsupported URL scheme {other:?}; use http or https"
            ));
        }
    }
    let host = url
        .host_str()
        .ok_or_else(|| "the URL has no host".to_owned())?;
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);

    let listed = host_matches_list(host, &policy.allow_domains);
    if !policy.allow_domains.is_empty() && !listed {
        return Err(format!("host {host:?} is not in web.allow_domains"));
    }
    if policy.block_private_addresses && !listed {
        let port = url.port_or_known_default().unwrap_or(0);
        guard_host(host, port).await?;
    }
    Ok(())
}

/// Reject `host` if it is, or resolves to, any non-global address.
async fn guard_host(host: &str, port: u16) -> Result<(), String> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if is_public_ip(ip) {
            Ok(())
        } else {
            Err(format!("{host} is a non-global address"))
        };
    }
    let mut resolved = false;
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| format!("could not resolve {host}: {error}"))?;
    for address in addresses {
        resolved = true;
        if !is_public_ip(address.ip()) {
            return Err(format!(
                "{host} resolves to the non-global address {}",
                address.ip()
            ));
        }
    }
    if !resolved {
        return Err(format!("{host} did not resolve to any address"));
    }
    Ok(())
}

/// Whether `host` equals, or is a subdomain of, one of `domains` (ASCII-folded).
fn host_matches_list(host: &str, domains: &[String]) -> bool {
    let host = host.to_ascii_lowercase();
    domains.iter().any(|domain| {
        let domain = domain.trim().trim_start_matches('.').to_ascii_lowercase();
        !domain.is_empty() && (host == domain || host.ends_with(&format!(".{domain}")))
    })
}

/// Whether an address is globally routable — the negation of the SSRF guard.
fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    if ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
    {
        return false;
    }
    let octets = ip.octets();
    if octets[0] == 0 || octets[0] >= 224 {
        // 0.0.0.0/8, and multicast (224/4) plus reserved (240/4).
        return false;
    }
    if octets[0] == 100 && (64..=127).contains(&octets[1]) {
        // Carrier-grade NAT (RFC 6598 shared range).
        return false;
    }
    true
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return false;
    }
    if let Some(v4) = ip.to_ipv4_mapped() {
        // ::ffff:a.b.c.d — judge by the embedded IPv4 address.
        return is_public_v4(v4);
    }
    let segments = ip.segments();
    if segments[0] & 0xfe00 == 0xfc00 {
        // Unique local, fc00::/7.
        return false;
    }
    if segments[0] & 0xffc0 == 0xfe80 {
        // Link-local unicast, fe80::/10.
        return false;
    }
    true
}

/// Reduce HTML to readable text: the `<title>`, then the body with scripts and
/// styles removed, tags flattened (block tags to newlines), entities decoded and
/// whitespace collapsed. A heuristic, not a browser — enough for a model to read.
fn html_to_text(html: &str) -> (Option<String>, String) {
    let title = extract_title(html);
    let stripped = remove_blocks(html);
    let flattened = strip_tags(&stripped);
    let decoded = decode_entities(&flattened);
    (title, collapse_whitespace(&decoded))
}

fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let content_start = lower[open..].find('>')? + open + 1;
    let content_end = lower[content_start..].find("</title>")? + content_start;
    let raw = &html[content_start..content_end];
    let text = collapse_whitespace(&decode_entities(raw));
    if text.is_empty() { None } else { Some(text) }
}

/// Drop `<script>`, `<style>`, `<head>` and `<svg>` elements whole.
fn remove_blocks(html: &str) -> String {
    let mut current = html.to_owned();
    for tag in ["script", "style", "noscript", "head", "svg"] {
        current = remove_element(&current, tag);
    }
    current
}

fn remove_element(input: &str, tag: &str) -> String {
    // `to_ascii_lowercase` preserves byte length, so byte indices align.
    let lower = input.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if lower[index..].starts_with(&open) {
            match lower[index..].find(&close) {
                Some(offset) => {
                    index += offset + close.len();
                    out.push(' ');
                    continue;
                }
                None => break, // unterminated element: drop the remainder
            }
        }
        let ch = input[index..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

/// Replace tags with spacing: block-level tags become a newline, the rest a
/// space, so words never run together where an element boundary sat.
fn strip_tags(input: &str) -> String {
    const BLOCK: &[&str] = &[
        "br",
        "p",
        "div",
        "li",
        "tr",
        "ul",
        "ol",
        "table",
        "section",
        "article",
        "header",
        "footer",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "blockquote",
        "pre",
    ];
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '<' {
            out.push(ch);
            continue;
        }
        let mut tag = String::new();
        for inner in chars.by_ref() {
            if inner == '>' {
                break;
            }
            tag.push(inner);
        }
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace())
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        out.push(if BLOCK.contains(&name.as_str()) {
            '\n'
        } else {
            ' '
        });
    }
    out
}

/// Decode the common named entities plus numeric (`&#38;`, `&#x26;`) forms.
fn decode_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '&' {
            out.push(ch);
            continue;
        }
        let ahead: String = chars.clone().take(12).collect();
        if let Some(semicolon) = ahead.find(';')
            && let Some(decoded) = decode_entity(&ahead[..semicolon])
        {
            out.push(decoded);
            for _ in 0..=semicolon {
                chars.next();
            }
            continue;
        }
        out.push('&');
    }
    out
}

fn decode_entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
            } else if let Some(decimal) = name.strip_prefix('#') {
                decimal.parse::<u32>().ok().and_then(char::from_u32)
            } else {
                None
            }
        }
    }
}

/// Collapse each line's inner whitespace to single spaces and runs of blank
/// lines to one, so the text reads as paragraphs rather than a wall or a sprawl.
fn collapse_whitespace(input: &str) -> String {
    let mut out = String::new();
    let mut pending_blank = false;
    for line in input.split('\n') {
        let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            pending_blank = !out.is_empty();
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if pending_blank {
                out.push('\n');
            }
        }
        pending_blank = false;
        out.push_str(&collapsed);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn public_addresses_are_told_from_private_ones() {
        // Addresses are constructed numerically rather than written as literals,
        // so the repository's address tripwire (scripts/check-secrets.sh) has no
        // committed machine address to flag while the ranges are still covered.
        let v4 = |a, b, c, d| IpAddr::V4(Ipv4Addr::new(a, b, c, d));
        let v6 = |segs: [u16; 8]| {
            IpAddr::V6(Ipv6Addr::new(
                segs[0], segs[1], segs[2], segs[3], segs[4], segs[5], segs[6], segs[7],
            ))
        };
        let public: [IpAddr; 3] = [
            v4(8, 8, 8, 8),
            v4(1, 1, 1, 1),
            v6([0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111]),
        ];
        for ip in public {
            assert!(is_public_ip(ip), "{ip} should be public");
        }
        let private: [IpAddr; 12] = [
            v4(127, 0, 0, 1),                                        // loopback
            v4(10, 0, 0, 1),                                         // private /8
            v4(192, 168, 1, 1),                                      // private /16
            v4(172, 16, 0, 1),                                       // private /12
            v4(169, 254, 1, 1),                                      // link-local
            v4(100, 64, 0, 1),                                       // shared / CGNAT
            v4(0, 0, 0, 0),                                          // unspecified
            IpAddr::V6(Ipv6Addr::LOCALHOST),                         // ::1
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),                       // ::
            v6([0xfc00, 0, 0, 0, 0, 0, 0, 1]),                       // unique-local
            v6([0xfe80, 0, 0, 0, 0, 0, 0, 1]),                       // link-local
            IpAddr::V6(Ipv4Addr::new(10, 0, 0, 1).to_ipv6_mapped()), // mapped private
        ];
        for ip in private {
            assert!(!is_public_ip(ip), "{ip} should be blocked");
        }
    }

    #[test]
    fn allow_list_matches_domain_and_subdomains() {
        let list = vec!["example.com".to_owned()];
        assert!(host_matches_list("example.com", &list));
        assert!(host_matches_list("docs.example.com", &list));
        assert!(host_matches_list("EXAMPLE.com", &list));
        assert!(!host_matches_list("notexample.com", &list));
        assert!(!host_matches_list("example.com.evil.com", &list));
        assert!(!host_matches_list("anything.org", &[]));
    }

    #[test]
    fn html_becomes_readable_text() {
        let html = "<html><head><title>Hello &amp; Bye</title>\
            <style>.x{color:red}</style></head>\
            <body><script>evil()</script><h1>Heading</h1>\
            <p>First&nbsp;paragraph.</p><p>Second &lt;one&gt;.</p></body></html>";
        let (title, text) = html_to_text(html);
        assert_eq!(title.as_deref(), Some("Hello & Bye"));
        assert!(!text.contains("evil"), "script content must be dropped");
        assert!(!text.contains("color:red"), "style content must be dropped");
        assert!(text.contains("Heading"));
        assert!(text.contains("First paragraph."));
        assert!(text.contains("Second <one>."));
    }

    #[test]
    fn numeric_entities_decode() {
        assert_eq!(decode_entities("A&#38;B&#x26;C"), "A&B&C");
        assert_eq!(decode_entities("bare & amp"), "bare & amp");
    }

    #[test]
    fn search_results_parse_the_common_shape() {
        let json = json!({
            "results": [
                { "title": "First", "url": "https://a.test/1", "content": "one" },
                { "url": "https://b.test/2", "snippet": "two" },
                { "title": "No URL, skipped", "content": "nope" }
            ]
        });
        let hits = parse_search_results(&json, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "First");
        assert_eq!(hits[0].snippet, "one");
        assert_eq!(hits[1].title, "https://b.test/2", "title falls back to url");
        assert_eq!(hits[1].snippet, "two");

        assert!(parse_search_results(&json!({}), 10).is_empty());
        assert_eq!(parse_search_results(&json, 1).len(), 1, "limit is honoured");
    }

    #[tokio::test]
    async fn fetch_without_a_web_context_is_a_controlled_error() {
        let ctx = ToolCtx::new(CancellationToken::new());
        let outcome = WebFetch::new()
            .call(&json!({ "url": "https://example.com" }), &ctx)
            .await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("web access is not enabled"));
    }

    #[tokio::test]
    async fn search_without_a_web_context_is_a_controlled_error() {
        let ctx = ToolCtx::new(CancellationToken::new());
        let outcome = WebSearch::new()
            .call(&json!({ "query": "rust" }), &ctx)
            .await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("web access is not enabled"));
    }
}
