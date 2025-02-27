use crate::utils::*;

use anyhow::{bail, Result};
use http::{HeaderMap, StatusCode, Version};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::atomic::{self, AtomicUsize},
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

static GLOBAL_ID: AtomicUsize = AtomicUsize::new(1);

#[derive(Debug, Clone, Serialize)]
pub struct Traffic {
    pub gid: usize,
    pub uri: String,
    pub method: String,
    pub req_headers: Option<Headers>,
    pub req_body_file: Option<String>,
    pub status: Option<u16>,
    pub http_version: Option<String>,
    pub res_headers: Option<Headers>,
    pub res_body_file: Option<String>,
    pub res_body_size: Option<u64>,
    pub websocket_id: Option<usize>,
    #[serde(serialize_with = "serialize_option_datetime")]
    pub start_time: Option<OffsetDateTime>,
    #[serde(serialize_with = "serialize_option_datetime")]
    pub end_time: Option<OffsetDateTime>,
    pub error: Option<String>,
    #[serde(skip)]
    pub(crate) valid: bool,
}

impl Traffic {
    pub fn new(uri: &str, method: &str) -> Self {
        Self {
            gid: GLOBAL_ID.fetch_add(1, atomic::Ordering::Relaxed),
            uri: uri.to_string(),
            method: method.to_string(),
            req_headers: None,
            req_body_file: None,
            status: None,
            http_version: None,
            res_headers: None,
            res_body_file: None,
            res_body_size: None,
            start_time: None,
            end_time: None,
            websocket_id: None,
            error: None,
            valid: true,
        }
    }

    pub fn add_error(&mut self, error: String) {
        match self.error.as_mut() {
            Some(current_error) => {
                current_error.push('\n');
                current_error.push_str(&error);
            }
            None => {
                self.error = Some(error);
            }
        }
    }

    pub fn oneline(&self) -> String {
        let mut output = format!("{} {}", self.method, self.uri,);
        if let Some(status) = self.status {
            output.push_str(&format!(" {}", status));
        }
        output
    }

    pub async fn markdown(&self) -> String {
        let (req_body, res_body) = self.bodies(false).await;

        let mut lines: Vec<String> = vec![];

        lines.push(format!("\n# {}", self.oneline()));

        if let Some(headers) = &self.req_headers {
            lines.push(render_header("REQUEST HEADERS", headers));
        }

        if let Some(body) = req_body {
            lines.push(render_body("REQUEST BODY", &body, &self.req_headers));
        }

        if let Some(headers) = &self.res_headers {
            lines.push(render_header("RESPONSE HEADERS", headers));
        }

        if let Some(body) = res_body {
            lines.push(render_body("RESPONSE BODY", &body, &self.res_headers));
        }

        if let Some(error) = &self.error {
            lines.push(render_error(error));
        }

        lines.join("\n\n")
    }

    pub async fn har(&self) -> Value {
        let entries = match self.har_entry().await {
            Some(v) => vec![v],
            None => vec![],
        };
        wrap_entries(entries)
    }

    pub async fn har_entry(&self) -> Option<Value> {
        let (req_body, res_body) = self.bodies(true).await;
        let http_version = self.http_version.clone().unwrap_or_default();
        let request = json!({
            "method": self.method,
            "url": self.uri,
            "httpVersion": http_version,
            "cookies": har_req_cookies(&self.req_headers),
            "headers": har_headers(&self.req_headers),
            "queryString": har_query_string(&self.uri),
            "postData": har_req_body(&req_body, &self.req_headers),
            "headersSize": har_size(self.req_headers.as_ref().map(|v| v.size), 0),
            "bodySize": har_size(req_body.as_ref().map(|v| v.size), 0),
        });
        let response = json!({
            "status": self.status.unwrap_or_default(),
            "statusText": "",
            "httpVersion": http_version,
            "cookies": har_res_cookies(&self.res_headers),
            "headers": har_headers(&self.res_headers),
            "content": har_res_body(&res_body, self.res_body_size.unwrap_or_default(), &self.res_headers),
            "redirectURL": get_header_value(&self.res_headers, "location").unwrap_or_default(),
            "headersSize": har_size(self.res_headers.as_ref().map(|v| v.size), -1),
            "bodySize": har_size(self.res_body_size, -1),
        });
        Some(json!({
            "startedDateTime": self.start_time.as_ref().and_then(|v| v.format(&Rfc3339).ok()),
            "time": self.time().map(|v| v as isize).unwrap_or(-1),
            "request": request,
            "response": response,
            "cache": {},
            "timings": {
                "connect": -1,
                "ssl": -1,
                "send": -1,
                "receive": -1,
                "wait": -1
            }
        }))
    }

    pub async fn curl(&self) -> String {
        let req_body = Body::read(&self.req_body_file, false).await;

        let mut output = format!("curl {}", self.uri);
        let escape_single_quote = |v: &str| v.replace('\'', r#"'\''"#);
        if self.method != "GET" {
            output.push_str(&format!(" \\\n  -X {}", self.method));
        }
        let headers = match &self.req_headers {
            Some(headers) => headers.items.as_slice(),
            None => &[],
        };
        for header in headers {
            if ["content-length", "host"].contains(&header.name.as_str()) {
                continue;
            }
            output.push_str(&format!(
                " \\\n  -H '{}: {}'",
                header.name,
                escape_single_quote(&header.value)
            ))
        }
        if let Some(body) = req_body {
            let value = shell_words::quote(&body.value);
            if body.is_utf8() {
                output.push_str(&format!(" \\\n  -d {value}"));
            } else {
                output.push_str(&format!(" \\\n  -t {value}"));
            }
        }
        output
    }

    pub async fn json(&self) -> Value {
        let mut value = json!(self);
        let (req_body, res_body) = self.bodies(true).await;
        value["req_body"] = json!(req_body);
        value["res_body"] = json!(res_body);
        value
    }

    pub async fn export(&self, format: &str) -> Result<(String, &'static str)> {
        match format {
            "markdown" => Ok((self.markdown().await, "text/markdown; charset=UTF-8")),
            "har" => Ok((
                serde_json::to_string_pretty(&self.har().await)?,
                "application/json; charset=UTF-8",
            )),
            "curl" => Ok((self.curl().await, "text/plain; charset=UTF-8")),
            "req-body" | "res-body" => {
                let body = match format {
                    "req-body" => Body::read(&self.req_body_file, false).await,
                    "res-body" => Body::read(&self.res_body_file, false).await,
                    _ => unreachable!(),
                };
                match body {
                    Some(body) => Ok((body.value.clone(), "text/plain; charset=UTF-8")),
                    _ => bail!("No {format} data"),
                }
            }
            "" => Ok((
                serde_json::to_string_pretty(&self.json().await)?,
                "application/json; charset=UTF-8",
            )),
            _ => bail!("Unsupported format: {}", format),
        }
    }

    pub(crate) fn head(&self, id: usize) -> TrafficHead {
        TrafficHead {
            id,
            method: self.method.clone(),
            uri: self.uri.clone(),
            status: self.status,
            size: self.res_body_size,
            time: self.time(),
            mime: extract_mime(&self.res_headers).to_string(),
            websocket_id: self.websocket_id,
        }
    }

    pub(crate) fn time(&self) -> Option<u64> {
        match (self.end_time, self.start_time) {
            (Some(end_time), Some(start_time)) => {
                let duration = end_time - start_time;
                Some(duration.whole_milliseconds() as u64)
            }
            _ => None,
        }
    }

    pub(crate) fn set_req_headers(&mut self, headers: &HeaderMap) -> &mut Self {
        self.req_headers = Some(Headers::new(headers));
        self
    }

    pub(crate) fn set_req_body_file(&mut self, path: &Path) -> &mut Self {
        self.req_body_file = Some(path.display().to_string());
        self
    }

    pub(crate) fn set_res_status(&mut self, status: StatusCode) -> &mut Self {
        self.status = Some(status.into());
        self
    }

    pub(crate) fn set_http_version(&mut self, http_version: &Version) -> &mut Self {
        self.http_version = Some(format!("{http_version:?}"));
        self
    }

    pub(crate) fn set_res_headers(&mut self, headers: &HeaderMap) -> &mut Self {
        self.res_headers = Some(Headers::new(headers));
        self
    }

    pub(crate) fn set_res_body_file(&mut self, path: &Path) -> &mut Self {
        self.res_body_file = Some(path.display().to_string());
        self
    }

    pub(crate) fn set_websocket_id(&mut self, id: usize) -> &mut Self {
        self.websocket_id = Some(id);
        self
    }

    pub(crate) fn check_match(&mut self, is_match: bool) -> &mut Self {
        self.valid = self.valid && is_match;
        self
    }

    pub(crate) fn set_start_time(&mut self) {
        self.start_time = Some(OffsetDateTime::now_utc());
    }

    pub(crate) async fn uncompress_res_file(&mut self) {
        let Some(path) = &self.res_body_file else {
            return;
        };
        let (new_path, encoding) = match ENCODING_EXTS
            .into_iter()
            .find_map(|(encoding, ext)| path.strip_suffix(ext).map(|v| (v, encoding)))
        {
            Some(v) => v,
            None => return,
        };
        let _ = uncompress_file(encoding, path, new_path).await;
        self.res_body_file = Some(new_path.to_string());
    }

    pub(crate) fn done_res_body(&mut self, raw_size: u64) {
        if raw_size == 0 {
            self.res_body_file = None;
        }
        if self.error.is_none() {
            self.end_time = Some(OffsetDateTime::now_utc());
            self.res_body_size = Some(raw_size);
        }
    }

    pub(crate) async fn bodies(&self, binary_in_base64: bool) -> (Option<Body>, Option<Body>) {
        tokio::join!(
            Body::read(&self.req_body_file, binary_in_base64),
            Body::read(&self.res_body_file, binary_in_base64)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrafficHead {
    pub id: usize,
    pub method: String,
    pub uri: String,
    pub status: Option<u16>,
    pub size: Option<u64>,
    pub time: Option<u64>,
    pub mime: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub websocket_id: Option<usize>,
}

impl TrafficHead {
    pub fn test_filter(&self, value: &str) -> bool {
        format!(
            "{} {} {} {}",
            self.uri,
            self.method,
            self.status
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "null".into()),
            self.mime
        )
        .contains(value)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Headers {
    pub items: Vec<Header>,
    pub size: u64,
}

impl Headers {
    pub fn new(headers: &HeaderMap) -> Self {
        Self {
            items: map_headers(headers),
            size: cal_headers_size(headers),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

impl Header {
    pub fn new(name: &str, value: &str) -> Self {
        Self {
            name: name.to_string(),
            value: value.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Body {
    pub encode: String,
    pub value: String,
    pub size: u64,
}

impl Body {
    pub async fn read(path: &Option<String>, binary_in_base64: bool) -> Option<Self> {
        let path = path.as_ref()?;

        let encoding = ENCODING_EXTS
            .into_iter()
            .find_map(|(encoding, ext)| path.strip_suffix(ext).map(|_| encoding));
        let ret = match encoding {
            Some(encoding) => {
                let data = uncompress_data(encoding, path).await.ok()?;
                if data.is_empty() {
                    return None;
                }
                if binary_in_base64 {
                    Self::bytes(&data)
                } else {
                    match std::str::from_utf8(&data) {
                        Ok(text) => Self::text(text),
                        Err(_) => Self::path(path),
                    }
                }
            }
            None => {
                if binary_in_base64 {
                    let data = tokio::fs::read(path).await.ok()?;
                    if data.is_empty() {
                        return None;
                    }
                    Self::bytes(&data)
                } else {
                    match tokio::fs::read_to_string(path).await {
                        Ok(text) => {
                            if text.is_empty() {
                                return None;
                            }
                            Self::text(&text)
                        }
                        Err(err) => {
                            if err.kind() != std::io::ErrorKind::InvalidData {
                                return None;
                            } else {
                                Self::path(path)
                            }
                        }
                    }
                }
            }
        };
        Some(ret)
    }

    pub fn bytes(data: &[u8]) -> Self {
        let size = data.len();
        match std::str::from_utf8(data) {
            Ok(text) => Self::text(text),
            Err(_) => Body {
                encode: "base64".to_string(),
                value: base64_encode(data),
                size: size as _,
            },
        }
    }

    pub fn text(text: &str) -> Self {
        Body {
            encode: "utf8".to_string(),
            value: text.to_string(),
            size: text.len() as _,
        }
    }

    pub fn path(path: &str) -> Self {
        Body {
            encode: "path".to_string(),
            value: path.to_string(),
            size: 0,
        }
    }

    pub fn is_utf8(&self) -> bool {
        self.encode == "utf8"
    }
}

fn render_header(title: &str, headers: &Headers) -> String {
    let value = headers
        .items
        .iter()
        .map(|header| format!("{}: {}", header.name, header.value))
        .collect::<Vec<String>>()
        .join("\n");
    format!(
        r#"{title}
```
{value}
```"#
    )
}

pub(crate) fn render_body(title: &str, body: &Body, headers: &Option<Headers>) -> String {
    let content_type = extract_mime(headers);
    let value = &body.value;
    if body.is_utf8() {
        let lang = to_md_lang(content_type);
        format!(
            r#"{title}
```{lang}
{value}
```"#
        )
    } else {
        format!("{title}\n\n[BINARY DATA]({value})")
    }
}

fn render_error(error: &str) -> String {
    if error.contains('\n') {
        format!(
            r#"ERROR
```
{}
```"#,
            error
        )
    } else {
        format!("ERROR: {}", error)
    }
}

fn har_headers(headers: &Option<Headers>) -> Value {
    match headers {
        Some(headers) => headers.items.iter().map(|header| json!(header)).collect(),
        None => json!([]),
    }
}

fn har_size(size: Option<u64>, default_value: i64) -> i64 {
    size.map(|v| v as i64).unwrap_or(default_value)
}

fn har_query_string(url: &str) -> Value {
    match url::Url::parse(url) {
        Ok(url) => url
            .query_pairs()
            .into_iter()
            .map(|(k, v)| json!({ "name": &k, "value": &v }))
            .collect(),
        Err(_) => json!([]),
    }
}

fn har_req_cookies(headers: &Option<Headers>) -> Value {
    match headers {
        Some(headers) => headers
            .items
            .iter()
            .filter(|header| header.name == "cookie")
            .flat_map(|header| {
                header
                    .value
                    .split(';')
                    .map(|v| v.trim())
                    .collect::<Vec<&str>>()
            })
            .filter_map(|value| {
                value
                    .split_once('=')
                    .map(|(k, v)| json!({ "name": k, "value": v }))
            })
            .collect(),
        None => json!([]),
    }
}

fn har_req_body(body: &Option<Body>, headers: &Option<Headers>) -> Value {
    let content_type = get_header_value(headers, "content-type").unwrap_or_default();
    match body {
        Some(body) => json!({"mimeType": content_type, "text": body.value}),
        None => json!({"mimeType": content_type, "text": ""}),
    }
}

fn har_res_body(body: &Option<Body>, raw_size: u64, headers: &Option<Headers>) -> Value {
    let content_type = get_header_value(headers, "content-type").unwrap_or_default();
    match body {
        Some(body) => {
            let mut value = json!({"size": raw_size, "mimeType": content_type, "text": body.value});
            if !body.is_utf8() {
                value["encoding"] = "base64".into();
            }
            value["compression"] = (body.size as isize - raw_size as isize).into();
            value
        }
        None => json!({"size": 0, "mimeType": content_type, "text": ""}),
    }
}

fn har_res_cookies(headers: &Option<Headers>) -> Value {
    match headers {
        Some(headers) => headers
            .items
            .iter()
            .filter(|header| header.name.as_str() == "set-cookie")
            .filter_map(|header| {
                cookie::Cookie::parse(&header.value).ok().map(|cookie| {
                    let mut json_cookie =
                        json!({ "name": cookie.name(), "value": cookie.value(), });
                    if let Some(value) = cookie.path() {
                        json_cookie["path"] = value.into();
                    }
                    if let Some(value) = cookie.domain() {
                        json_cookie["domain"] = value.into();
                    }
                    if let Some(cookie::Expiration::DateTime(datetime)) = cookie.expires() {
                        if let Ok(datetime) =
                            datetime.format(&time::format_description::well_known::Rfc3339)
                        {
                            json_cookie["expires"] = datetime.into();
                        }
                    }
                    if let Some(value) = cookie.http_only() {
                        json_cookie["httpOnly"] = value.into();
                    }
                    if let Some(value) = cookie.secure() {
                        json_cookie["secure"] = value.into();
                    }
                    json_cookie
                })
            })
            .collect(),
        None => json!([]),
    }
}

pub(crate) fn extract_mime(headers: &Option<Headers>) -> &str {
    get_header_value(headers, "content-type")
        .map(|v| match v.split_once(';') {
            Some((v, _)) => v.trim(),
            None => v,
        })
        .unwrap_or_default()
}

pub(crate) fn get_header_value<'a>(headers: &'a Option<Headers>, key: &str) -> Option<&'a str> {
    headers.as_ref().and_then(|v| {
        v.items
            .iter()
            .find(|header| header.name == key)
            .map(|header| header.value.as_str())
    })
}

pub(crate) fn wrap_entries(entries: Vec<Value>) -> Value {
    json!({
        "log": {
            "version": "1.2",
            "creator": {
                "name": "proxyfor",
                "version": env!("CARGO_PKG_VERSION"),
                "comment": "",
            },
            "pages": [],
            "entries": entries,
        }
    })
}

fn map_headers(headers: &HeaderMap) -> Vec<Header> {
    headers
        .iter()
        .map(|(key, value)| Header::new(key.as_str(), value.to_str().unwrap_or_default()))
        .collect()
}

fn cal_headers_size(headers: &HeaderMap) -> u64 {
    headers
        .iter()
        .map(|(key, value)| key.as_str().len() as u64 + value.len() as u64 + 12)
        .sum::<u64>()
        + 7
}
