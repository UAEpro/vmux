//! Browser / URL inspection helpers used by daemon request handlers.
use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::time::Instant;

use super::trim_output;

pub(crate) fn validate_url(url: &str) -> Result<()> {
    // This is a local dev tool: opening localhost/private previews is a core use
    // case, so we deliberately do NOT block private or loopback hosts. We only
    // harden parsing: reject empty input, whitespace/control characters, an
    // unsupported scheme, and a missing host (finding 15).
    if url.is_empty() {
        return Err(anyhow!("open-url requires a URL"));
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(anyhow!(
            "URL must not contain whitespace or control characters"
        ));
    }
    let lower = url.to_ascii_lowercase();
    let rest = if let Some(rest) = lower.strip_prefix("http://") {
        rest
    } else if let Some(rest) = lower.strip_prefix("https://") {
        rest
    } else {
        return Err(anyhow!("open-url only supports http:// and https:// URLs"));
    };
    // Host is everything up to the first path/query/fragment separator, minus
    // any userinfo and port.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() {
        return Err(anyhow!("URL is missing a host"));
    }
    Ok(())
}

pub(crate) fn url_open_command(url: &str) -> String {
    let browser = ["w3m", "lynx", "links", "elinks", "browsh"]
        .into_iter()
        .find(|candidate| command_exists(candidate));
    let argv = if let Some(browser) = browser {
        vec![browser.to_string(), url.to_string()]
    } else {
        vec![
            "curl".to_string(),
            "-L".to_string(),
            "--max-time".to_string(),
            "30".to_string(),
            url.to_string(),
        ]
    };
    shell_words::join(argv)
}

pub(crate) fn url_snapshot(url: &str) -> Result<serde_json::Value> {
    validate_url(url)?;
    let body = fetch_url_body(url, "url snapshot")?;
    let title = html_title(&body);
    let links = html_links(&body, url);
    let text = html_to_text(&body);
    Ok(serde_json::json!({
        "url": url,
        "title": title,
        "text": trim_output(text, 32_000),
        "links": links,
    }))
}

pub(crate) fn url_links(url: &str) -> Result<serde_json::Value> {
    let snapshot = url_snapshot(url)?;
    let links = snapshot
        .get("links")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    Ok(serde_json::json!({
        "url": url,
        "title": snapshot.get("title").cloned().unwrap_or(serde_json::Value::Null),
        "links": links,
    }))
}

pub(crate) fn url_forms(url: &str) -> Result<serde_json::Value> {
    validate_url(url)?;
    let body = fetch_url_body(url, "url forms")?;
    Ok(serde_json::json!({
        "url": url,
        "title": html_title(&body),
        "forms": html_forms(&body, url),
    }))
}

pub(crate) fn url_evaluate(url: &str, expression: &str) -> Result<serde_json::Value> {
    validate_url(url)?;
    let body = fetch_url_body(url, "url evaluate")?;
    let expression = expression.trim();
    if expression.is_empty() {
        return Err(anyhow!("browser evaluate expression cannot be empty"));
    }
    let links = html_links(&body, url);
    let forms = html_forms(&body, url);
    let value = evaluate_static_expression(expression, &body, &links, &forms)?;
    Ok(serde_json::json!({
        "url": url,
        "engine": "static-html",
        "expression": expression,
        "value": value,
    }))
}

pub(crate) fn url_console(url: &str) -> Result<serde_json::Value> {
    validate_url(url)?;
    let body = fetch_url_body(url, "url console")?;
    let scripts = html_scripts(&body, url);
    Ok(serde_json::json!({
        "url": url,
        "engine": "static-html",
        "scripts": scripts,
        "console_calls": html_console_calls(&body),
        "noscript": html_noscript_blocks(&body),
    }))
}

pub(crate) fn url_network(url: &str) -> Result<serde_json::Value> {
    validate_url(url)?;
    let started = Instant::now();
    let output = Command::new("curl")
        .arg("-L")
        .arg("--max-time")
        .arg("30")
        .arg("-sS")
        .arg("-D")
        .arg("-")
        .arg("-o")
        .arg("/dev/null")
        .arg("-w")
        .arg("\nVMUX_CURL_META\t%{http_code}\t%{url_effective}\t%{content_type}\t%{size_download}\t%{time_total}\t%{num_redirects}\n")
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("run curl for url network")?;
    let elapsed_ms = started.elapsed().as_millis();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let (headers, meta) = parse_curl_network_output(&stdout);
    if !output.status.success() {
        return Err(anyhow!(
            "url network failed: {}",
            if stderr.is_empty() {
                "curl failed"
            } else {
                &stderr
            }
        ));
    }
    Ok(serde_json::json!({
        "url": url,
        "elapsed_ms": elapsed_ms,
        "status": meta.status,
        "effective_url": meta.effective_url,
        "content_type": meta.content_type,
        "bytes": meta.bytes,
        "curl_time_total": meta.time_total,
        "redirects": meta.redirects,
        "headers": headers,
    }))
}

/// Hard cap on browser/fetch response bodies (bugs.md P1#8).
pub(crate) const FETCH_BODY_CAP: usize = 2 * 1024 * 1024;

pub(crate) fn fetch_url_body(url: &str, label: &str) -> Result<String> {
    let output = Command::new("curl")
        .arg("-L")
        .arg("--max-time")
        .arg("30")
        .arg("--max-filesize")
        .arg(FETCH_BODY_CAP.to_string())
        .arg("-sS")
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("run curl for {label}"))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow!(
            "{label} failed: {}",
            if error.is_empty() {
                "curl failed"
            } else {
                &error
            }
        ));
    }
    if output.stdout.len() > FETCH_BODY_CAP {
        return Err(anyhow!(
            "{label} response too large ({} bytes; max {FETCH_BODY_CAP})",
            output.stdout.len()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct CurlNetworkMeta {
    pub(crate) status: Option<u16>,
    pub(crate) effective_url: Option<String>,
    pub(crate) content_type: Option<String>,
    pub(crate) bytes: Option<u64>,
    pub(crate) time_total: Option<f64>,
    pub(crate) redirects: Option<u64>,
}

pub(crate) fn parse_curl_network_output(output: &str) -> (Vec<serde_json::Value>, CurlNetworkMeta) {
    let mut headers = Vec::new();
    let mut meta = CurlNetworkMeta::default();
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("VMUX_CURL_META\t") {
            let parts = rest.split('\t').collect::<Vec<_>>();
            meta.status = parts.first().and_then(|value| value.parse().ok());
            meta.effective_url = parts
                .get(1)
                .filter(|value| !value.is_empty())
                .map(|value| (*value).to_string());
            meta.content_type = parts
                .get(2)
                .filter(|value| !value.is_empty())
                .map(|value| (*value).to_string());
            meta.bytes = parts.get(3).and_then(|value| value.parse().ok());
            meta.time_total = parts.get(4).and_then(|value| value.parse().ok());
            meta.redirects = parts.get(5).and_then(|value| value.parse().ok());
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            if !name.is_empty() {
                headers.push(serde_json::json!({
                    "name": name,
                    "value": value.trim(),
                }));
            }
        }
    }
    (headers, meta)
}

pub(crate) fn evaluate_static_expression(
    expression: &str,
    html: &str,
    links: &[serde_json::Value],
    forms: &[serde_json::Value],
) -> Result<serde_json::Value> {
    match expression {
        "title" | "document.title" => Ok(html_title(html)
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null)),
        "text" | "document.body.innerText" | "body.innerText" => Ok(serde_json::Value::String(
            trim_output(html_to_text(html), 32_000),
        )),
        "links" => Ok(serde_json::Value::Array(links.to_vec())),
        "forms" => Ok(serde_json::Value::Array(forms.to_vec())),
        expression => {
            if let Some(index) = indexed_expression(expression, "links") {
                return links
                    .get(index)
                    .cloned()
                    .ok_or_else(|| anyhow!("link index {} out of range", index + 1));
            }
            if let Some(index) = indexed_expression(expression, "forms") {
                return forms
                    .get(index)
                    .cloned()
                    .ok_or_else(|| anyhow!("form index {} out of range", index + 1));
            }
            if let Some((index, field)) = indexed_field_expression(expression, "links") {
                return links
                    .get(index)
                    .and_then(|link| link.get(field))
                    .cloned()
                    .ok_or_else(|| anyhow!("link index {} has no field {field}", index + 1));
            }
            if let Some((index, field)) = indexed_field_expression(expression, "forms") {
                return forms
                    .get(index)
                    .and_then(|form| form.get(field))
                    .cloned()
                    .ok_or_else(|| anyhow!("form index {} has no field {field}", index + 1));
            }
            if let Some(selector) = expression.strip_prefix("text:") {
                return Ok(serde_json::Value::String(trim_output(
                    html_elements_text(html, selector).join("\n"),
                    32_000,
                )));
            }
            if let Some(selector) = expression.strip_prefix("selector:") {
                return Ok(serde_json::Value::Array(
                    html_elements_text(html, selector)
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect(),
                ));
            }
            Err(anyhow!(
                "unsupported static browser expression {expression}; try title, text, links, forms, links[1], links[1].href, text:h1, or selector:p"
            ))
        }
    }
}

pub(crate) fn indexed_expression(expression: &str, name: &str) -> Option<usize> {
    let rest = expression.strip_prefix(name)?.strip_prefix('[')?;
    let (index, suffix) = rest.split_once(']')?;
    if !suffix.is_empty() {
        return None;
    }
    one_based_index(index)
}

pub(crate) fn indexed_field_expression<'a>(
    expression: &'a str,
    name: &str,
) -> Option<(usize, &'a str)> {
    let rest = expression.strip_prefix(name)?.strip_prefix('[')?;
    let (index, suffix) = rest.split_once("].")?;
    let field = suffix.trim();
    if field.is_empty() {
        return None;
    }
    Some((one_based_index(index)?, field))
}

pub(crate) fn one_based_index(value: &str) -> Option<usize> {
    value.trim().parse::<usize>().ok()?.checked_sub(1)
}

pub(crate) fn html_scripts(html: &str, base_url: &str) -> Vec<serde_json::Value> {
    html_tag_blocks(html, "script")
        .into_iter()
        .enumerate()
        .map(|(index, (attrs, body))| {
            let src = html_attr(attrs, "src").map(|src| absolutize_url(base_url, &src));
            let inline = src.is_none();
            let script_type = html_attr(attrs, "type").unwrap_or_else(|| "text/javascript".into());
            serde_json::json!({
                "index": index + 1,
                "src": src,
                "type": script_type,
                "inline": inline,
                "bytes": body.len(),
                "preview": trim_output(body.trim().to_string(), 500),
            })
        })
        .collect()
}

pub(crate) fn html_console_calls(html: &str) -> Vec<serde_json::Value> {
    let mut calls = Vec::new();
    for (_, body) in html_tag_blocks(html, "script") {
        let mut rest = body.as_str();
        while let Some(index) = rest.find("console.") {
            rest = &rest[index + "console.".len()..];
            let method = rest
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect::<String>();
            if method.is_empty() {
                continue;
            }
            let preview = rest
                .find(';')
                .map(|end| &rest[..end])
                .unwrap_or(rest)
                .trim();
            calls.push(serde_json::json!({
                "method": method,
                "preview": trim_output(preview.to_string(), 500),
            }));
            rest = preview
                .len()
                .checked_add(1)
                .and_then(|offset| rest.get(offset..))
                .unwrap_or("");
        }
    }
    calls
}

pub(crate) fn html_noscript_blocks(html: &str) -> Vec<String> {
    html_tag_blocks(html, "noscript")
        .into_iter()
        .map(|(_, body)| html_to_text(&body))
        .filter(|text| !text.trim().is_empty())
        .collect()
}

pub(crate) fn html_tag_blocks<'a>(html: &'a str, tag: &str) -> Vec<(&'a str, String)> {
    let mut blocks = Vec::new();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut rest = html;
    while let Some(index) = rest.to_ascii_lowercase().find(&open) {
        rest = &rest[index + open.len()..];
        let Some(tag_end) = rest.find('>') else {
            break;
        };
        let attrs = &rest[..tag_end];
        let after = &rest[tag_end + 1..];
        let lower_after = after.to_ascii_lowercase();
        let Some(body_end) = lower_after.find(&close) else {
            break;
        };
        blocks.push((attrs, after[..body_end].to_string()));
        rest = &after[body_end + close.len()..];
    }
    blocks
}

pub(crate) fn html_elements_text(html: &str, selector: &str) -> Vec<String> {
    let selector = selector.trim().trim_start_matches('.');
    if selector.is_empty()
        || !selector
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Vec::new();
    }
    html_tag_blocks(html, selector)
        .into_iter()
        .map(|(_, body)| html_to_text(&body))
        .filter(|text| !text.trim().is_empty())
        .collect()
}

pub(crate) fn form_default_fields(form: &serde_json::Value) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    if let Some(fields) = form.get("fields").and_then(|fields| fields.as_array()) {
        for field in fields {
            let Some(name) = field.get("name").and_then(|name| name.as_str()) else {
                continue;
            };
            let value = field
                .get("value")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            values.insert(name.to_string(), value.to_string());
        }
    }
    values
}

pub(crate) fn form_submission_target(
    action: &str,
    method: &str,
    values: &BTreeMap<String, String>,
) -> Result<String> {
    match method {
        "get" => Ok(url_with_query(action, values)),
        "post" => {
            let mut argv = vec![
                "curl".to_string(),
                "-L".to_string(),
                "-X".to_string(),
                "POST".to_string(),
            ];
            for (name, value) in values {
                argv.push("--data-urlencode".to_string());
                argv.push(format!("{name}={value}"));
            }
            argv.push(action.to_string());
            Ok(shell_words::join(argv))
        }
        other => Err(anyhow!("unsupported form method {other}")),
    }
}

pub(crate) fn url_with_query(url: &str, values: &BTreeMap<String, String>) -> String {
    if values.is_empty() {
        return url.to_string();
    }
    let query = values
        .iter()
        .map(|(name, value)| format!("{}={}", url_encode(name), url_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let separator = if url.contains('?') { "&" } else { "?" };
    format!("{url}{separator}{query}")
}

pub(crate) fn url_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else if byte == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub(crate) fn compact_url_title(url: &str) -> String {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    rest.split('/').next().unwrap_or(rest).to_string()
}

pub(crate) fn command_exists(command: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(command).is_file())
}

pub(crate) fn html_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let after_tag = html[start..].find('>')? + start + 1;
    let end = lower[after_tag..].find("</title>")? + after_tag;
    let title = html_unescape(&html[after_tag..end]).trim().to_string();
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

pub(crate) fn html_links(html: &str, base_url: &str) -> Vec<serde_json::Value> {
    let mut links = Vec::new();
    let mut rest = html;
    while let Some(index) = rest.to_ascii_lowercase().find("<a ") {
        rest = &rest[index + 3..];
        let Some(end) = rest.find('>') else {
            break;
        };
        let attrs = &rest[..end];
        if let Some(href) = html_attr(attrs, "href") {
            let after = &rest[end + 1..];
            let lower_after = after.to_ascii_lowercase();
            let text = lower_after
                .find("</a>")
                .map(|end| html_to_text(&after[..end]))
                .unwrap_or_default();
            links.push(serde_json::json!({
                "href": absolutize_url(base_url, &href),
                "text": text.trim(),
            }));
        }
        rest = &rest[end + 1..];
    }
    links
}

pub(crate) fn html_forms(html: &str, base_url: &str) -> Vec<serde_json::Value> {
    let mut forms = Vec::new();
    let mut rest = html;
    while let Some(index) = rest.to_ascii_lowercase().find("<form") {
        rest = &rest[index + 5..];
        let Some(tag_end) = rest.find('>') else {
            break;
        };
        let attrs = &rest[..tag_end];
        let after = &rest[tag_end + 1..];
        let lower_after = after.to_ascii_lowercase();
        let body_end = lower_after.find("</form>").unwrap_or(after.len());
        let body = &after[..body_end];
        let method = html_attr(attrs, "method")
            .unwrap_or_else(|| "get".to_string())
            .to_ascii_lowercase();
        let action = html_attr(attrs, "action")
            .map(|action| absolutize_url(base_url, &action))
            .unwrap_or_else(|| base_url.to_string());
        let label = html_attr(attrs, "aria-label")
            .or_else(|| html_attr(attrs, "name"))
            .or_else(|| html_attr(attrs, "id"));
        forms.push(serde_json::json!({
            "index": forms.len() + 1,
            "method": method,
            "action": action,
            "label": label,
            "fields": html_form_fields(body),
        }));
        rest = if body_end < after.len() {
            &after[body_end + "</form>".len()..]
        } else {
            ""
        };
    }
    forms
}

pub(crate) fn html_form_fields(html: &str) -> Vec<serde_json::Value> {
    let mut fields = Vec::new();
    let mut rest = html;
    while let Some(index) = rest.to_ascii_lowercase().find("<input") {
        rest = &rest[index + 6..];
        let Some(end) = rest.find('>') else {
            break;
        };
        let attrs = &rest[..end];
        if let Some(name) = html_attr(attrs, "name") {
            let field_type = html_attr(attrs, "type").unwrap_or_else(|| "text".to_string());
            if !matches!(field_type.as_str(), "submit" | "button" | "reset" | "image") {
                fields.push(serde_json::json!({
                    "name": name,
                    "type": field_type,
                    "value": html_attr(attrs, "value").unwrap_or_default(),
                }));
            }
        }
        rest = &rest[end + 1..];
    }

    let mut rest = html;
    while let Some(index) = rest.to_ascii_lowercase().find("<textarea") {
        rest = &rest[index + 9..];
        let Some(end) = rest.find('>') else {
            break;
        };
        let attrs = &rest[..end];
        let after = &rest[end + 1..];
        let lower_after = after.to_ascii_lowercase();
        let value_end = lower_after.find("</textarea>").unwrap_or(0);
        if let Some(name) = html_attr(attrs, "name") {
            fields.push(serde_json::json!({
                "name": name,
                "type": "textarea",
                "value": html_unescape(&after[..value_end]).trim(),
            }));
        }
        rest = if value_end < after.len() {
            &after[value_end + "</textarea>".len()..]
        } else {
            ""
        };
    }

    let mut rest = html;
    while let Some(index) = rest.to_ascii_lowercase().find("<select") {
        rest = &rest[index + 7..];
        let Some(end) = rest.find('>') else {
            break;
        };
        let attrs = &rest[..end];
        let after = &rest[end + 1..];
        let lower_after = after.to_ascii_lowercase();
        let body_end = lower_after.find("</select>").unwrap_or(0);
        if let Some(name) = html_attr(attrs, "name") {
            fields.push(serde_json::json!({
                "name": name,
                "type": "select",
                "value": selected_option_value(&after[..body_end]).unwrap_or_default(),
            }));
        }
        rest = if body_end < after.len() {
            &after[body_end + "</select>".len()..]
        } else {
            ""
        };
    }

    fields
}

pub(crate) fn selected_option_value(html: &str) -> Option<String> {
    let mut first = None;
    let mut rest = html;
    while let Some(index) = rest.to_ascii_lowercase().find("<option") {
        rest = &rest[index + 7..];
        let Some(end) = rest.find('>') else {
            break;
        };
        let attrs = &rest[..end];
        let value = html_attr(attrs, "value").unwrap_or_else(|| {
            let after = &rest[end + 1..];
            let lower_after = after.to_ascii_lowercase();
            lower_after
                .find("</option>")
                .map(|end| html_to_text(&after[..end]))
                .unwrap_or_default()
                .trim()
                .to_string()
        });
        if first.is_none() {
            first = Some(value.clone());
        }
        if attrs.to_ascii_lowercase().contains("selected") {
            return Some(value);
        }
        rest = &rest[end + 1..];
    }
    first
}

pub(crate) fn html_attr(attrs: &str, name: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let needle = format!("{name}={quote}");
        if let Some(start) = attrs.to_ascii_lowercase().find(&needle) {
            let value_start = start + needle.len();
            let value_end = attrs[value_start..].find(quote)? + value_start;
            return Some(html_unescape(&attrs[value_start..value_end]));
        }
    }
    None
}

pub(crate) fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut entity = String::new();
    let mut in_entity = false;
    for ch in html.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                out.push(' ');
            }
            continue;
        }
        if in_entity {
            if ch == ';' {
                out.push_str(&html_unescape_entity(&entity));
                entity.clear();
                in_entity = false;
            } else if entity.len() < 16 {
                entity.push(ch);
            } else {
                out.push('&');
                out.push_str(&entity);
                entity.clear();
                in_entity = false;
            }
            continue;
        }
        match ch {
            '<' => in_tag = true,
            '&' => in_entity = true,
            ch => out.push(ch),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn html_unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '&' {
            out.push(ch);
            continue;
        }
        let mut entity = String::new();
        while let Some(next) = chars.peek().copied() {
            if next == ';' {
                chars.next();
                out.push_str(&html_unescape_entity(&entity));
                entity.clear();
                break;
            }
            if entity.len() >= 16 || !(next.is_ascii_alphanumeric() || next == '#') {
                out.push('&');
                out.push_str(&entity);
                entity.clear();
                break;
            }
            entity.push(next);
            chars.next();
        }
        if !entity.is_empty() {
            out.push('&');
            out.push_str(&entity);
        }
    }
    out
}

pub(crate) fn html_unescape_entity(entity: &str) -> String {
    match entity {
        "amp" => "&".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "quot" => "\"".to_string(),
        "apos" => "'".to_string(),
        _ => format!("&{entity};"),
    }
}

pub(crate) fn absolutize_url(base_url: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    let Some((scheme, rest)) = base_url.split_once("://") else {
        return href.to_string();
    };
    let host = rest.split('/').next().unwrap_or(rest);
    if href.starts_with('/') {
        format!("{scheme}://{host}{href}")
    } else {
        let base_dir = rest
            .rsplit_once('/')
            .map(|(dir, _)| dir)
            .filter(|dir| dir.contains('/'))
            .unwrap_or(host);
        format!("{scheme}://{base_dir}/{href}")
    }
}
