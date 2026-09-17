use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Result;
use reqwest::Client;
use serde_json::Value;

use super::{Tool, ToolContext, ToolOutput};

const TIMEOUT: Duration = Duration::from_secs(30);
/// 响应体下载上限（字节），超过即截断。
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
/// 返回给模型的正文上限（字符）。
const MAX_TEXT_CHARS: usize = 20_000;
const USER_AGENT: &str = concat!("rust-ai-agent/", env!("CARGO_PKG_VERSION"));

fn client() -> Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            Client::builder()
                .timeout(TIMEOUT)
                .user_agent(USER_AGENT)
                .build()
                .expect("reqwest client with rustls")
        })
        .clone()
}

/// 基本 SSRF 防护（借鉴 Claude Code 对 WebFetch 的主机限制）：
/// 拒绝非 http(s) scheme 与形似本机/内网的 host 字符串。这是启发式检查，
/// 不解析 DNS，生产环境应叠加出站网络策略。
fn forbidden_host(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Some(format!("unsupported scheme '{scheme}' (only http/https)"));
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    let blocked = host == "localhost"
        || host == "0.0.0.0"
        || host == "::1"
        || host == "[::1]"
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.starts_with("127.")
        || host.starts_with("10.")
        || host.starts_with("192.168.")
        || host.starts_with("169.254.")
        || is_private_172(&host);
    blocked.then(|| format!("host '{host}' looks like a local/private address"))
}

/// 172.16.0.0 – 172.31.255.255 私有网段。
fn is_private_172(host: &str) -> bool {
    // 去掉 "172." 前缀后剩三段：second.third.fourth。
    let Some(rest) = host.strip_prefix("172.") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('.').collect();
    parts.len() == 3
        && parts
            .first()
            .and_then(|part| part.parse::<u8>().ok())
            .is_some_and(|second| (16..=31).contains(&second))
        && parts.iter().all(|part| part.parse::<u8>().is_ok())
}

/// 从 URL 提取 host，作为权限规则的匹配内容。
fn host_of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    // 带 IPv6 方括号的 host 转小写后原样返回。
    parsed.host_str().map(|host| host.to_ascii_lowercase())
}

/// 块级标签前缀：命中时插入换行，保持基本的段落结构。
/// 匹配对象是带 '>' 的完整小写标签，因此 "</p>" 不会误伤 "</pre>"。
const BLOCK_BREAK_TAGS: [&str; 14] = [
    "</p>", "<br", "</div>", "</li>", "</tr>", "</h", "<h1", "<h2", "<h3", "<h4", "<h5", "<h6",
    "</ul>", "</ol>",
];

/// 极简 HTML → 文本：去掉 script/style 内容与标签、解码常见实体、
/// 压缩空白。不追求保真，只给模型可读的正文。
fn html_to_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len() / 2);
    let mut rest = html;
    while let Some(offset) = rest.find('<') {
        text.push_str(&rest[..offset]);
        // 所有标签内部的索引都相对 tag_rest，切片也一律基于 tag_rest。
        let tag_rest = &rest[offset..];
        let lower_tag = tag_rest.to_ascii_lowercase();

        // script/style：连内容一起丢弃，跳到闭合标签之后。
        if let Some(tag) = ["<script", "<style"]
            .into_iter()
            .find(|tag| lower_tag.starts_with(tag))
        {
            let close = format!("</{}>", &tag[1..]);
            let after = lower_tag
                .find(&close)
                .and_then(|end| lower_tag[end..].find('>').map(|gap| end + gap + 1))
                .unwrap_or(lower_tag.len());
            rest = &tag_rest[after..];
            continue;
        }

        match tag_rest.find('>') {
            Some(end) => {
                let tag = &lower_tag[..end + 1];
                if BLOCK_BREAK_TAGS
                    .iter()
                    .any(|break_tag| tag.starts_with(break_tag))
                {
                    text.push('\n');
                }
                rest = &tag_rest[end + 1..];
            }
            // 未闭合的孤立 '<'：剩余部分按纯文本处理。
            None => {
                text.push_str(tag_rest);
                rest = "";
            }
        }
    }
    text.push_str(rest);

    let decoded = decode_entities(&text);
    // 空白折叠：连续空白压成一个空格；换行是硬分隔，保留且不被空格覆盖。
    let mut collapsed = String::with_capacity(decoded.len());
    #[derive(PartialEq)]
    enum Pending {
        None,
        Space,
        Newline,
    }
    let mut pending = Pending::None;
    for ch in decoded.chars() {
        match ch {
            '\n' => pending = Pending::Newline,
            c if c.is_whitespace() => {
                if pending == Pending::None {
                    pending = Pending::Space;
                }
            }
            c => {
                match pending {
                    Pending::Newline => collapsed.push('\n'),
                    Pending::Space => collapsed.push(' '),
                    Pending::None => {}
                }
                pending = Pending::None;
                collapsed.push(c);
            }
        }
    }
    collapsed.trim().to_string()
}

fn decode_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&mdash;", "—")
        .replace("&hellip;", "…")
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str("\n[truncated]");
    out
}

/// 抓取网页并抽取正文文本，对应 Claude Code 的 WebFetch（只读）。
/// 权限规则按域名匹配：`WebFetch(domain:example.com)`。
pub struct WebFetch;

#[async_trait::async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &'static str {
        "WebFetch"
    }

    fn description(&self) -> &'static str {
        "Fetch content from an http(s) URL and return readable text. HTML pages are \
         stripped to plain text (scripts/styles removed, whitespace collapsed); JSON \
         and plain text are returned as-is. Response bodies are capped at 2 MB and the \
         returned text at 20,000 characters. Requests time out after 30s. Local/private \
         network hosts are refused. Requires permission per domain: allow with a rule \
         like WebFetch(domain:example.com)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The http(s) URL to fetch"
                }
            },
            "required": ["url"]
        })
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn rule_contents(&self, input: &Value) -> Vec<String> {
        input
            .get("url")
            .and_then(|v| v.as_str())
            .and_then(host_of)
            .map(|host| vec![format!("domain:{host}")])
            .unwrap_or_default()
    }

    async fn call(&self, input: Value, _ctx: &mut ToolContext) -> Result<ToolOutput> {
        let Some(url) = input.get("url").and_then(|v| v.as_str()) else {
            return Ok(ToolOutput::err("missing required parameter: url"));
        };
        let url = url.trim();
        if let Some(reason) = forbidden_host(url) {
            return Ok(ToolOutput::err(format!(
                "refusing to fetch {url}: {reason}"
            )));
        }

        let response = match client().get(url).send().await {
            Ok(response) => response,
            Err(error) => return Ok(ToolOutput::err(format!("request to {url} failed: {error}"))),
        };
        // 重定向后再次校验最终主机，防止跨域跳转绕过上面的检查。
        let final_url = response.url().as_str().to_string();
        if let Some(reason) = forbidden_host(&final_url) {
            return Ok(ToolOutput::err(format!(
                "redirect to {final_url} refused: {reason}"
            )));
        }
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();

        // 流式读取并截断到 MAX_BODY_BYTES。
        let mut body: Vec<u8> = Vec::new();
        let mut stream = response;
        while let Some(chunk) = stream.chunk().await? {
            let remaining = MAX_BODY_BYTES.saturating_sub(body.len());
            if remaining == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            if body.len() >= MAX_BODY_BYTES {
                break;
            }
        }

        let raw = String::from_utf8_lossy(&body).to_string();
        let text = if content_type.contains("html") {
            html_to_text(&raw)
        } else {
            raw
        };
        let text = truncate_chars(&text, MAX_TEXT_CHARS);

        Ok(ToolOutput::ok(format!(
            "URL: {final_url}\nHTTP status: {}\nContent-Type: {}\n\n{}",
            status.as_u16(),
            if content_type.is_empty() {
                "(none)"
            } else {
                &content_type
            },
            text
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_hosts_are_rejected() {
        for url in [
            "http://localhost/x",
            "http://127.0.0.1/x",
            "https://10.0.0.5/x",
            "https://192.168.1.1/x",
            "https://172.16.0.1/x",
            "https://172.31.255.255/x",
            "https://169.254.1.1/x",
            "ftp://example.com/x",
            "file:///etc/passwd",
            "https://printer.local/x",
        ] {
            assert!(
                forbidden_host(url).is_some(),
                "expected {url} to be blocked"
            );
        }
        assert!(forbidden_host("https://example.com/x").is_none());
        assert!(forbidden_host("https://172.32.0.1/x").is_none());
        assert!(forbidden_host("https://172.160.0.1/x").is_none());
        // 非 4 段的 172.* 不视为私网。
        assert!(forbidden_host("https://172.16.example.com/x").is_none());
    }

    #[test]
    fn rule_content_is_domain() {
        let tool = WebFetch;
        let content = tool.rule_contents(&serde_json::json!({"url": "https://Example.COM/path"}));
        assert_eq!(content, vec!["domain:example.com".to_string()]);
        assert!(tool
            .rule_contents(&serde_json::json!({"url": "not a url"}))
            .is_empty());
    }

    #[test]
    fn html_to_text_strips_scripts_and_tags() {
        let html = r#"<html><head><style>body{color:red}</style><script>alert(1)</script></head>
<body><h1>Title</h1><p>Hello &amp;   world</p><!-- comment --><br/><p>Second</p></body></html>"#;
        let text = html_to_text(html);
        assert!(!text.to_lowercase().contains("script"));
        assert!(!text.to_lowercase().contains("alert"));
        assert!(!text.to_lowercase().contains("style"));
        assert!(!text.contains('<'));
        assert!(text.contains("Title"));
        assert!(text.contains("Hello & world"));
        assert!(text.contains("Second"));
    }

    #[test]
    fn html_to_text_collapses_whitespace() {
        let text = html_to_text("<p>a\n\n\n  b</p>   <p>c</p>");
        assert_eq!(text, "a\nb\nc");
    }

    #[test]
    fn truncate_marks_truncation() {
        let short = "abc".repeat(100);
        assert_eq!(truncate_chars(&short, 500), short);
        let long = "ab".repeat(20_000);
        let truncated = truncate_chars(&long, MAX_TEXT_CHARS);
        assert!(truncated.ends_with("[truncated]"));
        assert!(truncated.chars().count() <= MAX_TEXT_CHARS + "\n[truncated]".len());
    }

    #[tokio::test]
    async fn missing_url_is_rejected() {
        let mut ctx = ToolContext {
            working_dir: std::env::temp_dir(),
            read_state: crate::tools::ReadFileState::new(),
            output_dir: std::env::temp_dir().join("out"),
            session_id: "s".to_string(),
            todos: Vec::new(),
        };
        let output = WebFetch
            .call(serde_json::json!({}), &mut ctx)
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("missing required parameter"));
    }

    #[tokio::test]
    async fn local_url_is_refused_without_network() {
        let mut ctx = ToolContext {
            working_dir: std::env::temp_dir(),
            read_state: crate::tools::ReadFileState::new(),
            output_dir: std::env::temp_dir().join("out"),
            session_id: "s".to_string(),
            todos: Vec::new(),
        };
        let output = WebFetch
            .call(serde_json::json!({"url": "http://127.0.0.1:9/x"}), &mut ctx)
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("refusing to fetch"));
    }
}
