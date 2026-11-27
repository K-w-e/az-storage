//! Data-Plane Client: Blob & Queue REST (SharedKey / Bearer / SAS).

use crate::model::{AuthCredential, BlobItem, BlobListing, BlobPreview, QueueInfo, QueueMessage};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac};
use reqwest::{Method, Response, StatusCode, Url};
use sha2::Sha256;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

pub const API_VERSION: &str = "2023-11-03";
pub const PREVIEW_BYTES: u64 = 512 * 1024;
/// Below this threshold, uploads use a single Put Blob.
pub const SINGLE_PUT_MAX: u64 = 32 * 1024 * 1024;
pub const BLOCK_SIZE: usize = 8 * 1024 * 1024;

pub fn content_type_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "json" => "application/json",
        "txt" | "log" | "md" => "text/plain",
        "csv" => "text/csv",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "text/javascript",
        "yaml" | "yml" => "application/yaml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
}

/// Service endpoint and credential used to sign requests.
pub struct Target<'a> {
    pub endpoint: &'a str,
    pub cred: &'a AuthCredential,
}

// ---------------------------------------------------------------------------
// SharedKey (HMAC-SHA256)
// ---------------------------------------------------------------------------

/// Standard HTTP headers included in the StringToSign.
#[derive(Default)]
pub struct StdHeaders<'a> {
    pub content_length: &'a str,
    pub content_type: &'a str,
    pub if_none_match: &'a str,
    pub range: &'a str,
}

pub fn string_to_sign(
    verb: &str,
    std_h: &StdHeaders,
    ms_headers: &[(String, String)],
    account: &str,
    url_path: &str,
    query: &[(String, String)],
) -> String {
    let mut headers: Vec<(String, String)> = ms_headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    headers.sort();
    let canon_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();

    let mut q: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .collect();
    q.sort();
    let mut canon_resource = format!("/{account}{url_path}");
    for (k, v) in q {
        canon_resource.push_str(&format!("\n{k}:{v}"));
    }

    // VERB, Content-Encoding, Content-Language, Content-Length, Content-MD5,
    // Content-Type, Date, If-Modified-Since, If-Match, If-None-Match,
    // If-Unmodified-Since, Range
    let StdHeaders {
        content_length,
        content_type,
        if_none_match,
        range,
    } = std_h;
    format!(
        "{verb}\n\n\n{content_length}\n\n{content_type}\n\n\n\n{if_none_match}\n\n{range}\n{canon_headers}{canon_resource}"
    )
}

pub fn sign(account_key: &str, sts: &str) -> Result<String> {
    let key = B64
        .decode(account_key.trim())
        .context("AccountKey is not valid base64")?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).map_err(|e| anyhow!("{e}"))?;
    mac.update(sts.as_bytes());
    Ok(B64.encode(mac.finalize().into_bytes()))
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct StorageClient {
    http: reqwest::Client,
}

fn encode_path(p: &str) -> String {
    p.split('/')
        .map(|s| urlencoding::encode(s).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

impl StorageClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    async fn raw(
        &self,
        method: Method,
        t: &Target<'_>,
        path: &str,
        query: &[(&str, String)],
        extra_ms: &[(&str, String)],
    ) -> Result<Response> {
        self.raw_body(method, t, path, query, extra_ms, None, None)
            .await
    }

    /// Executes a signed request. `path` is the unencoded resource path
    /// (e.g. "/container/dir/file.json"), and `query` contains unencoded pairs.
    #[allow(clippy::too_many_arguments)]
    async fn raw_body(
        &self,
        method: Method,
        t: &Target<'_>,
        path: &str,
        query: &[(&str, String)],
        extra_ms: &[(&str, String)],
        body: Option<Vec<u8>>,
        if_none_match: Option<&str>,
    ) -> Result<Response> {
        let base =
            Url::parse(t.endpoint).with_context(|| format!("invalid endpoint: {}", t.endpoint))?;
        let url_path = format!("{}{}", base.path().trim_end_matches('/'), encode_path(path));
        let query: Vec<(String, String)> = query
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();

        let mut qs: Vec<String> = query
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect();
        if let AuthCredential::SasToken(sas) = t.cred {
            qs.push(sas.clone());
        }
        let mut url = format!(
            "{}://{}{}",
            base.scheme(),
            base.host_str().unwrap_or_default(),
            base.port().map(|p| format!(":{p}")).unwrap_or_default()
        );
        url.push_str(&url_path);
        if !qs.is_empty() {
            url.push('?');
            url.push_str(&qs.join("&"));
        }

        let date = chrono::Utc::now()
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        let mut ms: Vec<(String, String)> = vec![
            ("x-ms-date".into(), date),
            ("x-ms-version".into(), API_VERSION.into()),
        ];
        ms.extend(extra_ms.iter().map(|(k, v)| (k.to_string(), v.clone())));

        let has_body = matches!(method, Method::PUT | Method::POST);
        let mut req = self.http.request(method.clone(), &url);
        for (k, v) in &ms {
            req = req.header(k.as_str(), v.as_str());
        }
        let body_len = body.as_ref().map(|b| b.len()).unwrap_or(0);
        if has_body || body.is_some() {
            req = req
                .header(reqwest::header::CONTENT_LENGTH, body_len.to_string())
                .body(body.unwrap_or_default());
        }
        if let Some(inm) = if_none_match {
            req = req.header(reqwest::header::IF_NONE_MATCH, inm);
        }
        match t.cred {
            AuthCredential::SharedKey {
                account_name,
                account_key,
            } => {
                let cl = if body_len > 0 {
                    body_len.to_string()
                } else {
                    String::new()
                };
                let std_h = StdHeaders {
                    content_length: &cl,
                    if_none_match: if_none_match.unwrap_or(""),
                    ..Default::default()
                };
                let sts = string_to_sign(
                    method.as_str(),
                    &std_h,
                    &ms,
                    account_name,
                    &url_path,
                    &query,
                );
                let sig = sign(account_key, &sts)?;
                req = req.header("Authorization", format!("SharedKey {account_name}:{sig}"));
            }
            AuthCredential::BearerToken(tok) => req = req.bearer_auth(tok),
            AuthCredential::SasToken(_) => {}
        }
        Ok(req.send().await?)
    }

    async fn request(
        &self,
        method: Method,
        t: &Target<'_>,
        path: &str,
        query: &[(&str, String)],
        extra_ms: &[(&str, String)],
    ) -> Result<Response> {
        let resp = self.raw(method, t, path, query, extra_ms).await?;
        check(resp).await
    }

    // ------------------------------ Blob -----------------------------------

    pub async fn list_containers(&self, t: &Target<'_>) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut marker = String::new();
        loop {
            let mut q = vec![("comp", "list".to_string())];
            if !marker.is_empty() {
                q.push(("marker", marker.clone()));
            }
            let doc = parse_xml(
                &self
                    .request(Method::GET, t, "/", &q, &[])
                    .await?
                    .text()
                    .await?,
            )?;
            if let Some(cs) = doc.child("Containers") {
                out.extend(cs.children_named("Container").map(|c| c.text_of("Name")));
            }
            marker = doc.text_of("NextMarker");
            if marker.is_empty() {
                break;
            }
        }
        Ok(out)
    }

    pub async fn list_blobs(
        &self,
        t: &Target<'_>,
        container: &str,
        prefix: &str,
    ) -> Result<BlobListing> {
        self.list_blobs_inner(t, container, prefix, true).await
    }

    /// Recursively lists all blobs under `prefix` (without a delimiter).
    pub async fn list_blobs_flat(
        &self,
        t: &Target<'_>,
        container: &str,
        prefix: &str,
    ) -> Result<Vec<BlobItem>> {
        Ok(self
            .list_blobs_inner(t, container, prefix, false)
            .await?
            .blobs)
    }

    async fn list_blobs_inner(
        &self,
        t: &Target<'_>,
        container: &str,
        prefix: &str,
        delimiter: bool,
    ) -> Result<BlobListing> {
        let mut out = BlobListing::default();
        let mut marker = String::new();
        loop {
            let mut q = vec![
                ("restype", "container".to_string()),
                ("comp", "list".to_string()),
            ];
            if delimiter {
                q.push(("delimiter", "/".to_string()));
            }
            if !prefix.is_empty() {
                q.push(("prefix", prefix.to_string()));
            }
            if !marker.is_empty() {
                q.push(("marker", marker.clone()));
            }
            let path = format!("/{container}");
            let doc = parse_xml(
                &self
                    .request(Method::GET, t, &path, &q, &[])
                    .await?
                    .text()
                    .await?,
            )?;
            if let Some(blobs) = doc.child("Blobs") {
                for n in &blobs.children {
                    match n.name.as_str() {
                        "BlobPrefix" => out.prefixes.push(n.text_of("Name")),
                        "Blob" => {
                            let p = n.child("Properties");
                            let prop = |k: &str| p.map(|p| p.text_of(k)).unwrap_or_default();
                            out.blobs.push(BlobItem {
                                name: n.text_of("Name"),
                                size: prop("Content-Length").parse().unwrap_or(0),
                                content_type: prop("Content-Type"),
                                last_modified: prop("Last-Modified"),
                                blob_type: prop("BlobType"),
                                etag: prop("Etag"),
                            });
                        }
                        _ => {}
                    }
                }
            }
            marker = doc.text_of("NextMarker");
            if marker.is_empty() {
                break;
            }
        }
        Ok(out)
    }

    // ---------------------------- Transfers --------------------------------

    /// Stream a blob to `dest` (via a `.part` file followed by a rename).
    pub async fn download_blob(
        &self,
        t: &Target<'_>,
        container: &str,
        name: &str,
        dest: &Path,
        cancel: &AtomicBool,
        mut progress: impl FnMut(u64),
    ) -> Result<()> {
        use futures::StreamExt;
        use tokio::io::AsyncWriteExt;
        let path = format!("/{container}/{name}");
        let resp = self.request(Method::GET, t, &path, &[], &[]).await?;
        if let Some(dir) = dest.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let fname = dest.file_name().and_then(|f| f.to_str()).unwrap_or("blob");
        let tmp = dest.with_file_name(format!(".{fname}.part"));
        let mut file = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("creating {}", tmp.display()))?;
        let mut stream = resp.bytes_stream();
        let res: Result<()> = async {
            while let Some(chunk) = stream.next().await {
                if cancel.load(Ordering::Relaxed) {
                    bail!("cancelled");
                }
                let chunk = chunk?;
                file.write_all(&chunk).await?;
                progress(chunk.len() as u64);
            }
            file.flush().await?;
            Ok(())
        }
        .await;
        drop(file);
        match res {
            Ok(()) => {
                tokio::fs::rename(&tmp, dest).await?;
                Ok(())
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                Err(e)
            }
        }
    }

    /// Get Blob Properties (HEAD): `None` if the blob does not exist, otherwise its size.
    pub async fn blob_size(
        &self,
        t: &Target<'_>,
        container: &str,
        name: &str,
    ) -> Result<Option<u64>> {
        let resp = self
            .raw(Method::HEAD, t, &format!("/{container}/{name}"), &[], &[])
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = check(resp).await?;
        Ok(Some(
            resp.headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        ))
    }

    /// Uploads a local file as a Block Blob. Small files use a single
    /// Put Blob; large files are uploaded in blocks (Put Block + Put Block List).
    /// Returns `false` if the blob exists and `overwrite` is disabled.
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_file(
        &self,
        t: &Target<'_>,
        container: &str,
        name: &str,
        src: &Path,
        overwrite: bool,
        cancel: &AtomicBool,
        mut progress: impl FnMut(u64),
    ) -> Result<bool> {
        use tokio::io::AsyncReadExt;
        let path = format!("/{container}/{name}");
        let size = tokio::fs::metadata(src)
            .await
            .with_context(|| format!("reading {}", src.display()))?
            .len();
        let ctype = ("x-ms-blob-content-type", content_type_for(name).to_string());
        let inm = (!overwrite).then_some("*");

        if size <= SINGLE_PUT_MAX {
            let data = tokio::fs::read(src).await?;
            let ms = [("x-ms-blob-type", "BlockBlob".to_string()), ctype];
            let resp = self
                .raw_body(Method::PUT, t, &path, &[], &ms, Some(data), inm)
                .await?;
            if !overwrite && matches!(resp.status().as_u16(), 409 | 412) {
                return Ok(false);
            }
            check(resp).await?;
            progress(size);
            return Ok(true);
        }

        if !overwrite && self.blob_size(t, container, name).await?.is_some() {
            return Ok(false);
        }
        let mut file = tokio::fs::File::open(src).await?;
        let mut ids = Vec::new();
        loop {
            if cancel.load(Ordering::Relaxed) {
                bail!("cancelled");
            }
            let mut buf = Vec::with_capacity(BLOCK_SIZE);
            (&mut file)
                .take(BLOCK_SIZE as u64)
                .read_to_end(&mut buf)
                .await?;
            if buf.is_empty() {
                break;
            }
            let len = buf.len() as u64;
            let id = B64.encode(format!("block-{:08}", ids.len()));
            let q = [("comp", "block".to_string()), ("blockid", id.clone())];
            check(
                self.raw_body(Method::PUT, t, &path, &q, &[], Some(buf), None)
                    .await?,
            )
            .await?;
            ids.push(id);
            progress(len);
        }
        let xml: String = ids
            .iter()
            .map(|id| format!("<Latest>{id}</Latest>"))
            .collect();
        let body =
            format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>{xml}</BlockList>");
        let q = [("comp", "blocklist".to_string())];
        let resp = self
            .raw_body(
                Method::PUT,
                t,
                &path,
                &q,
                &[ctype],
                Some(body.into_bytes()),
                inm,
            )
            .await?;
        if !overwrite && matches!(resp.status().as_u16(), 409 | 412) {
            return Ok(false);
        }
        check(resp).await?;
        Ok(true)
    }

    pub async fn preview_blob(
        &self,
        t: &Target<'_>,
        container: &str,
        name: &str,
    ) -> Result<BlobPreview> {
        let path = format!("/{container}/{name}");
        let range = format!("bytes=0-{}", PREVIEW_BYTES - 1);
        let resp = self
            .raw(Method::GET, t, &path, &[], &[("x-ms-range", range)])
            .await?;
        if resp.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            // Empty blob.
            return Ok(BlobPreview {
                data: Vec::new(),
                truncated: false,
            });
        }
        let resp = check(resp).await?;
        let total: Option<u64> = resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next())
            .and_then(|v| v.parse().ok());
        let mut data = resp.bytes().await?.to_vec();
        data.truncate(PREVIEW_BYTES as usize);
        let truncated = total.is_some_and(|t| t > data.len() as u64);
        Ok(BlobPreview { data, truncated })
    }

    pub async fn delete_blob(&self, t: &Target<'_>, container: &str, name: &str) -> Result<()> {
        let path = format!("/{container}/{name}");
        self.request(
            Method::DELETE,
            t,
            &path,
            &[],
            &[("x-ms-delete-snapshots", "include".into())],
        )
        .await?;
        Ok(())
    }

    // ------------------------------ Queue ----------------------------------

    pub async fn list_queues(&self, t: &Target<'_>) -> Result<Vec<QueueInfo>> {
        let mut names = Vec::new();
        let mut marker = String::new();
        loop {
            let mut q = vec![("comp", "list".to_string())];
            if !marker.is_empty() {
                q.push(("marker", marker.clone()));
            }
            let doc = parse_xml(
                &self
                    .request(Method::GET, t, "/", &q, &[])
                    .await?
                    .text()
                    .await?,
            )?;
            if let Some(qs) = doc.child("Queues") {
                names.extend(qs.children_named("Queue").map(|c| c.text_of("Name")));
            }
            marker = doc.text_of("NextMarker");
            if marker.is_empty() {
                break;
            }
        }
        Ok(self.queues_with_counts(t, names).await)
    }

    /// Approximate message count (Get Queue Metadata), fetched in parallel.
    pub async fn queues_with_counts(&self, t: &Target<'_>, names: Vec<String>) -> Vec<QueueInfo> {
        let counts = futures::future::join_all(names.iter().map(|n| self.queue_count(t, n))).await;
        names
            .into_iter()
            .zip(counts)
            .map(|(name, c)| QueueInfo {
                name,
                approx_count: c.ok(),
            })
            .collect()
    }

    async fn queue_count(&self, t: &Target<'_>, queue: &str) -> Result<u64> {
        let resp = self
            .request(
                Method::GET,
                t,
                &format!("/{queue}"),
                &[("comp", "metadata".into())],
                &[],
            )
            .await?;
        resp.headers()
            .get("x-ms-approximate-messages-count")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| anyhow!("message count is missing"))
    }

    pub async fn peek_messages(&self, t: &Target<'_>, queue: &str) -> Result<Vec<QueueMessage>> {
        let q = [
            ("peekonly", "true".to_string()),
            ("numofmessages", "32".to_string()),
        ];
        let doc = self.get_messages(t, queue, &q).await?;
        Ok(doc.into_iter().map(|(m, _)| m).collect())
    }

    async fn get_messages(
        &self,
        t: &Target<'_>,
        queue: &str,
        q: &[(&str, String)],
    ) -> Result<Vec<(QueueMessage, String)>> {
        let path = format!("/{queue}/messages");
        let doc = parse_xml(
            &self
                .request(Method::GET, t, &path, q, &[])
                .await?
                .text()
                .await?,
        )?;
        Ok(doc
            .children_named("QueueMessage")
            .map(|m| {
                (
                    QueueMessage {
                        id: m.text_of("MessageId"),
                        insertion_time: m.text_of("InsertionTime"),
                        expiration_time: m.text_of("ExpirationTime"),
                        dequeue_count: m.text_of("DequeueCount").parse().unwrap_or(0),
                        text: m.text_of("MessageText"),
                    },
                    m.text_of("PopReceipt"),
                )
            })
            .collect())
    }

    /// Peek does not return the `popreceipt`: to delete a specific message,
    /// retrieve it with Get Messages (brief visibility), delete the target,
    /// and immediately make the other messages visible again.
    pub async fn delete_message(&self, t: &Target<'_>, queue: &str, id: &str) -> Result<()> {
        for _ in 0..16 {
            let q = [
                ("numofmessages", "32".to_string()),
                ("visibilitytimeout", "30".to_string()),
            ];
            let batch = self.get_messages(t, queue, &q).await?;
            if batch.is_empty() {
                break;
            }
            let mut found = false;
            for (m, receipt) in &batch {
                let path = format!("/{queue}/messages/{}", m.id);
                if m.id == id {
                    self.request(
                        Method::DELETE,
                        t,
                        &path,
                        &[("popreceipt", receipt.clone())],
                        &[],
                    )
                    .await?;
                    found = true;
                } else {
                    let q = [
                        ("popreceipt", receipt.clone()),
                        ("visibilitytimeout", "0".to_string()),
                    ];
                    let _ = self.request(Method::PUT, t, &path, &q, &[]).await;
                }
            }
            if found {
                return Ok(());
            }
        }
        bail!("message {id} not found (already consumed or invisible)")
    }

    pub async fn clear_queue(&self, t: &Target<'_>, queue: &str) -> Result<()> {
        self.request(Method::DELETE, t, &format!("/{queue}/messages"), &[], &[])
            .await?;
        Ok(())
    }
}

async fn check(resp: Response) -> Result<Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let code_hdr = resp
        .headers()
        .get("x-ms-error-code")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = resp.text().await.unwrap_or_default();
    let (code, msg) = match parse_xml(&body) {
        Ok(doc) => (doc.text_of("Code"), doc.text_of("Message")),
        Err(_) => (String::new(), String::new()),
    };
    let code = if code.is_empty() {
        code_hdr.unwrap_or_default()
    } else {
        code
    };
    let msg = msg.lines().next().unwrap_or("").to_string();
    bail!("HTTP {} {} {}", status.as_u16(), code, msg)
}

// ---------------------------------------------------------------------------
// Mini DOM XML (quick-xml)
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct XmlNode {
    pub name: String,
    pub text: String,
    pub children: Vec<XmlNode>,
}

impl XmlNode {
    pub fn child(&self, name: &str) -> Option<&XmlNode> {
        self.children.iter().find(|c| c.name == name)
    }
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a XmlNode> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }
    pub fn text_of(&self, name: &str) -> String {
        self.child(name).map(|c| c.text.clone()).unwrap_or_default()
    }
}

pub fn parse_xml(s: &str) -> Result<XmlNode> {
    use quick_xml::events::Event;
    let s = s.trim_start_matches('\u{feff}');
    let mut reader = quick_xml::Reader::from_str(s);
    let mut stack: Vec<XmlNode> = vec![XmlNode::default()];
    loop {
        match reader.read_event()? {
            Event::Start(e) => stack.push(XmlNode {
                name: String::from_utf8_lossy(e.name().as_ref()).into_owned(),
                ..Default::default()
            }),
            Event::Empty(e) => {
                let n = XmlNode {
                    name: String::from_utf8_lossy(e.name().as_ref()).into_owned(),
                    ..Default::default()
                };
                stack.last_mut().unwrap().children.push(n);
            }
            Event::End(_) => {
                let n = stack.pop().unwrap();
                match stack.last_mut() {
                    Some(p) => p.children.push(n),
                    None => bail!("malformed XML"),
                }
            }
            Event::Text(t) => {
                let txt = t.unescape()?;
                stack.last_mut().unwrap().text.push_str(&txt);
            }
            Event::CData(c) => {
                stack
                    .last_mut()
                    .unwrap()
                    .text
                    .push_str(&String::from_utf8_lossy(&c.into_inner()));
            }
            Event::Eof => break,
            _ => {}
        }
    }
    // Return the document's root element.
    let mut root = stack.pop().ok_or_else(|| anyhow!("empty XML"))?;
    if root.children.len() == 1 {
        Ok(root.children.pop().unwrap())
    } else {
        bail!("XML has no root element")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_known_vector() {
        // StringToSign assembled manually according to the documentation.
        let ms = vec![
            ("x-ms-version".to_string(), "2023-11-03".to_string()),
            (
                "x-ms-date".to_string(),
                "Fri, 26 Jun 2015 23:39:12 GMT".to_string(),
            ),
        ];
        let q = vec![
            ("comp".to_string(), "list".to_string()),
            ("restype".to_string(), "container".to_string()),
        ];
        let sts = string_to_sign(
            "GET",
            &StdHeaders::default(),
            &ms,
            "myaccount",
            "/mycontainer",
            &q,
        );
        assert_eq!(
            sts,
            "GET\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:Fri, 26 Jun 2015 23:39:12 GMT\nx-ms-version:2023-11-03\n/myaccount/mycontainer\ncomp:list\nrestype:container"
        );
        assert!(sign("a2V5", &sts).is_ok());
    }

    /// Requires Azurite to be running: `cargo test -- --ignored azurite`
    #[tokio::test]
    #[ignore]
    async fn azurite_roundtrip() {
        let acct =
            crate::auth::parse_connection_string("dev", "UseDevelopmentStorage=true").unwrap();
        let cred = acct.credential.clone().unwrap();
        let c = StorageClient::new(reqwest::Client::new());
        let b = Target {
            endpoint: &acct.blob_endpoint,
            cred: &cred,
        };
        let q = Target {
            endpoint: &acct.queue_endpoint,
            cred: &cred,
        };
        let cs = c.list_containers(&b).await.unwrap();
        println!("containers {cs:?}");
        let root = c.list_blobs(&b, &cs[0], "").await.unwrap();
        println!(
            "root {:?} {:?}",
            root.prefixes,
            root.blobs.iter().map(|b| &b.name).collect::<Vec<_>>()
        );
        let blob = &root.blobs[0].name;
        let p = c.preview_blob(&b, &cs[0], blob).await.unwrap();
        println!("preview {}", String::from_utf8_lossy(&p.data));
        let sub = c.list_blobs(&b, &cs[0], &root.prefixes[0]).await.unwrap();
        println!("sub {:?}", sub.prefixes);
        let qs = c.list_queues(&q).await.unwrap();
        println!("queues {qs:?}");
        let msgs = c.peek_messages(&q, &qs[0].name).await.unwrap();
        println!("msgs {}", msgs.len());
        c.delete_message(&q, &qs[0].name, &msgs[1].id)
            .await
            .unwrap();
        let after = c.peek_messages(&q, &qs[0].name).await.unwrap();
        assert_eq!(after.len(), msgs.len() - 1);
        c.delete_blob(&b, &cs[0], blob).await.unwrap();
    }

    /// Upload (single and block) and download against Azurite.
    #[tokio::test]
    #[ignore]
    async fn azurite_transfer() {
        let acct =
            crate::auth::parse_connection_string("dev", "UseDevelopmentStorage=true").unwrap();
        let cred = acct.credential.clone().unwrap();
        let c = StorageClient::new(reqwest::Client::new());
        let b = Target {
            endpoint: &acct.blob_endpoint,
            cred: &cred,
        };
        let dir = std::env::temp_dir().join("az-storage-xfer");
        std::fs::create_dir_all(&dir).unwrap();
        let no = AtomicBool::new(false);
        for (name, size) in [
            ("small file.txt", 1000usize),
            ("big.bin", 40 * 1024 * 1024 + 123),
        ] {
            let data: Vec<u8> = (0..size).map(|i| (i * 31 % 251) as u8).collect();
            let src = dir.join(name);
            std::fs::write(&src, &data).unwrap();
            let mut sent = 0;
            assert!(
                c.upload_file(&b, "demo", &format!("up/{name}"), &src, true, &no, |n| {
                    sent += n
                })
                .await
                .unwrap()
            );
            assert_eq!(sent, size as u64);
            // Without overwrite, the existing blob should be skipped.
            assert!(
                !c.upload_file(&b, "demo", &format!("up/{name}"), &src, false, &no, |_| {})
                    .await
                    .unwrap()
            );
            assert_eq!(
                c.blob_size(&b, "demo", &format!("up/{name}"))
                    .await
                    .unwrap(),
                Some(size as u64)
            );
            let dest = dir.join(format!("dl-{name}"));
            c.download_blob(&b, "demo", &format!("up/{name}"), &dest, &no, |_| {})
                .await
                .unwrap();
            assert_eq!(std::fs::read(&dest).unwrap(), data);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Read-only test against live Azure (requires `az login`): `cargo test -- --ignored azure_live`
    #[tokio::test]
    #[ignore]
    async fn azure_live_readonly() {
        use crate::{arm, auth};
        let http = reqwest::Client::new();
        let tok = auth::get_access_token(auth::ARM_RESOURCE, None)
            .await
            .unwrap()
            .token;
        let subs = arm::list_subscriptions(&http, &tok).await.unwrap();
        println!("subs {}", subs.len());
        for s in &subs {
            let accts = match arm::list_storage_accounts(&http, &tok, s).await {
                Ok(a) => a,
                Err(e) => {
                    println!("{}: {e}", s.name);
                    continue;
                }
            };
            println!("{}: {} accounts", s.name, accts.len());
            if let Some(a) = accts.first() {
                let cred = match arm::list_keys(&http, &tok, &a.id).await {
                    Ok(k) => AuthCredential::SharedKey {
                        account_name: a.name.clone(),
                        account_key: k,
                    },
                    Err(e) => {
                        println!("listKeys failed: {e}");
                        AuthCredential::BearerToken(
                            auth::get_access_token(auth::STORAGE_RESOURCE, None)
                                .await
                                .unwrap()
                                .token,
                        )
                    }
                };
                let c = StorageClient::new(http.clone());
                let t = Target {
                    endpoint: &a.blob_endpoint,
                    cred: &cred,
                };
                println!(
                    "{} [{}] containers: {:?}",
                    a.name,
                    cred.label(),
                    c.list_containers(&t).await.map(|v| v.len())
                );
                let t = Target {
                    endpoint: &a.queue_endpoint,
                    cred: &cred,
                };
                println!(
                    "{} queues: {:?}",
                    a.name,
                    c.list_queues(&t).await.map(|v| v.len())
                );
                return;
            }
        }
    }

    #[test]
    fn parses_blob_listing() {
        let xml = "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?><EnumerationResults><Blobs><BlobPrefix><Name>a/</Name></BlobPrefix><Blob><Name>b &amp; c.txt</Name><Properties><Content-Length>12</Content-Length></Properties></Blob><BlobPrefix><Name>z/</Name></BlobPrefix></Blobs><NextMarker /></EnumerationResults>";
        let doc = parse_xml(xml).unwrap();
        let blobs = doc.child("Blobs").unwrap();
        assert_eq!(blobs.children.len(), 3);
        assert_eq!(blobs.children[1].text_of("Name"), "b & c.txt");
        assert_eq!(doc.text_of("NextMarker"), "");
    }
}
