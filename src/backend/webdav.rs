use super::{Backend, Entry, Progress, parse_endpoint};
use anyhow::{Context, Result, anyhow, bail};
use httpdate::parse_http_date;
use percent_encoding::percent_decode_str;
use reqwest::blocking::{Body, Client, Response};
use reqwest::{Method, StatusCode, Url};
use std::io::{self, Cursor, Read};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getlastmodified/><d:getcontentlength/></d:prop></d:propfind>"#;
const MAX_RETRIES: usize = 3;
const RETRY_BASE_DELAY_MS: u64 = 500;

pub struct WebDav {
    client: Client,
    auth: Option<(String, Option<String>)>,
    /// WebDAV API 入口
    endpoint: Url,
    /// endpoint + root 拼接后的远端仓库根目录
    base: Url,
    /// 追加到 endpoint 后的根目录路径，按层级创建
    root: String,
}

impl WebDav {
    pub fn new(url: &str, root: &str, username: &str, password: &str) -> Result<Self> {
        let endpoint = parse_endpoint(url)?;
        let mut base = endpoint.clone();
        {
            let mut segments = base
                .path_segments_mut()
                .map_err(|_| anyhow!("远端地址不支持路径：{url}"))?;
            for s in root.split('/').filter(|s| !s.is_empty()) {
                segments.push(s);
            }
        }
        let client = Client::builder()
            .user_agent(concat!("anysync/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        let auth = (!username.is_empty()).then(|| {
            (
                username.to_string(),
                (!password.is_empty()).then_some(password.to_string()),
            )
        });
        Ok(Self {
            client,
            auth,
            endpoint,
            base,
            root: root.to_string(),
        })
    }

    fn req(&self, method: Method, url: Url) -> reqwest::blocking::RequestBuilder {
        let builder = self.client.request(method, url);
        match &self.auth {
            Some((user, pass)) => builder.basic_auth(user, pass.clone()),
            None => builder,
        }
    }

    fn send_with_retry<F>(&self, mut build: F) -> Result<Response>
    where
        F: FnMut() -> reqwest::blocking::RequestBuilder,
    {
        for retry in 0..=MAX_RETRIES {
            match build().send() {
                Ok(resp) => {
                    let status = resp.status();
                    if retry < MAX_RETRIES && is_retryable_status(status) {
                        drop(resp);
                        thread::sleep(retry_delay(retry));
                        continue;
                    }
                    return Ok(resp);
                }
                Err(err) if retry < MAX_RETRIES => {
                    thread::sleep(retry_delay(retry));
                    let _ = err;
                }
                Err(err) => return Err(err.into()),
            }
        }
        unreachable!()
    }

    fn url_for(&self, rel: &str) -> Url {
        append_path(self.base.clone(), rel)
    }

    fn root_url_for(&self, rel: &str) -> Url {
        append_path(self.endpoint.clone(), rel)
    }

    pub fn get(&self, rel: &str) -> Result<Vec<u8>> {
        let resp = self.send_with_retry(|| self.req(Method::GET, self.url_for(rel)))?;
        if !resp.status().is_success() {
            bail!("下载失败 {}：HTTP {}", rel, resp.status().as_u16());
        }
        Ok(resp.bytes()?.to_vec())
    }

    /// 上传单个文件，返回（成功时的尝试次数，服务端最终状态码）。
    /// 状态码要带给调用方：服务端返回 2xx 却没保存时，它是唯一的诊断线索。
    pub fn put_with_progress<F>(&self, rel: &str, data: &[u8], progress: F) -> Result<(usize, u16)>
    where
        F: Fn(usize, u64, u64) + Send + Sync + 'static,
    {
        let progress: Arc<dyn Fn(usize, u64, u64) + Send + Sync> = Arc::new(progress);
        let total = data.len() as u64;
        for retry in 0..=MAX_RETRIES {
            let attempt = retry + 1;
            progress(attempt, 0, total);
            let reader = ProgressReader {
                reader: Cursor::new(data.to_vec()),
                attempt,
                read: 0,
                total,
                progress: Arc::clone(&progress),
            };
            match self
                .req(Method::PUT, self.url_for(rel))
                .body(Body::sized(reader, total))
                .send()
            {
                // 本函数不因文件暂未出现在远端列表而重传，仅处理明确的瞬时错误。
                Ok(resp) if resp.status().is_success() => {
                    return Ok((attempt, resp.status().as_u16()));
                }
                Ok(resp) => {
                    let status = resp.status();
                    if retry < MAX_RETRIES && is_retryable_status(status) {
                        drop(resp);
                        thread::sleep(retry_delay(retry));
                        continue;
                    }
                    bail!(
                        "上传失败 {rel}：HTTP {}{}",
                        status.as_u16(),
                        http_hint(&status)
                    );
                }
                // 仅连接阶段失败可确认请求未送达；其余传输错误的结果不确定，
                // 不重传以避免在服务端已保存时产生副本。
                Err(err) if retry < MAX_RETRIES && err.is_connect() => {
                    thread::sleep(retry_delay(retry));
                }
                Err(err) if err.is_connect() => {
                    return Err(err).with_context(|| {
                        format!("连续 {} 次连接服务器失败，未上传 {rel}", MAX_RETRIES + 1)
                    });
                }
                Err(err) => bail!("上传结果不确定 {rel}：{err}；为避免产生同名副本，未自动重传"),
            }
        }
        unreachable!()
    }

    /// 创建远端目录；405 表示已存在，忽略。
    pub fn mkcol(&self, rel: &str) -> Result<()> {
        self.mkcol_at(self.url_for(rel), rel)
    }

    fn mkcol_at(&self, url: Url, rel: &str) -> Result<()> {
        let method = Method::from_bytes(b"MKCOL").expect("MKCOL 是合法的 HTTP 方法");
        let resp = self.send_with_retry(|| self.req(method.clone(), url.clone()))?;
        if resp.status().as_u16() == 405 {
            return Ok(());
        }
        ensure_success(resp, "创建目录", rel)
    }

    /// 递归创建配置的远端根目录；已存在的目录由 405 忽略。
    pub fn ensure_root(&self) -> Result<()> {
        let paths = path_prefixes(&self.root);
        if paths.is_empty() {
            self.mkcol_at(self.endpoint.clone(), "")?;
        } else {
            for path in paths {
                self.mkcol_at(self.root_url_for(&path), &path)?;
            }
        }
        Ok(())
    }

    /// 递归列出远端所有文件（不含目录、不含根目录自身）。
    pub fn list_recursive(&self) -> Result<Vec<Entry>> {
        let mut files = Vec::new();
        let mut queue = vec![String::new()];
        while let Some(dir) = queue.pop() {
            for e in self.list(&dir)? {
                if e.path == dir {
                    continue;
                }
                if e.is_dir {
                    queue.push(e.path);
                } else {
                    files.push(e);
                }
            }
        }
        Ok(files)
    }

    fn propfind(&self, rel: &str, depth: &str) -> Result<Vec<Entry>> {
        let method = Method::from_bytes(b"PROPFIND").expect("PROPFIND 是合法的 HTTP 方法");
        let resp = self.send_with_retry(|| {
            self.req(method.clone(), self.url_for(rel))
                .header("Depth", depth)
                .header("Content-Type", "application/xml")
                .body(PROPFIND_BODY)
        })?;
        let status = resp.status();
        if status.as_u16() == 404 {
            return Ok(Vec::new()); // 远端目录或文件尚不存在
        }
        if !status.is_success() {
            bail!(
                "列出目录失败 {rel:?}：HTTP {}{}",
                status.as_u16(),
                http_hint(&status)
            );
        }
        let base_path = self.base.path().trim_end_matches('/').to_string();
        let text = resp.text()?;
        // 排查服务器 quirks 用：ANYSYNC_DEBUG=1 anysync pull 2>debug.log
        if std::env::var_os("ANYSYNC_DEBUG").is_some() {
            eprintln!("== PROPFIND {rel:?} Depth:{depth} ==\n{text}");
        }
        parse_multistatus(&text, &base_path)
    }

    /// Depth:1 列出单个目录。
    fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        self.propfind(dir, "1")
    }
}

impl Backend for WebDav {
    fn list_recursive(&self) -> Result<Vec<Entry>> {
        WebDav::list_recursive(self)
    }

    fn download(&self, path: &str) -> Result<Vec<u8>> {
        self.get(path)
    }

    fn upload(&self, path: &str, data: &[u8], progress: Progress) -> Result<usize> {
        self.put_with_progress(path, data, move |attempt, bytes, total| {
            progress(attempt, bytes, total);
        })
        .map(|(attempt, _status)| attempt)
    }

    fn ensure_root(&self) -> Result<()> {
        WebDav::ensure_root(self)
    }

    fn create_dir(&self, path: &str) -> Result<()> {
        self.mkcol(path)
    }
}

/// 给常见 HTTP 状态码补充人话提示。
fn http_hint(status: &reqwest::StatusCode) -> &'static str {
    match status.as_u16() {
        401 => "（认证失败，请检查用户名或密码）",
        409 => "（父目录不存在，请检查根目录配置）",
        _ => "",
    }
}

struct ProgressReader {
    reader: Cursor<Vec<u8>>,
    attempt: usize,
    read: u64,
    total: u64,
    progress: Arc<dyn Fn(usize, u64, u64) + Send + Sync>,
}

impl Read for ProgressReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = self.reader.read(buf)?;
        self.read += count as u64;
        if count > 0 {
            (self.progress)(self.attempt, self.read, self.total);
        }
        Ok(count)
    }
}

fn append_path(mut url: Url, rel: &str) -> Url {
    if let Ok(mut segments) = url.path_segments_mut() {
        for segment in rel.split('/').filter(|s| !s.is_empty()) {
            segments.push(segment);
        }
    }
    url
}

fn path_prefixes(path: &str) -> Vec<String> {
    let mut prefixes = Vec::new();
    let mut current = String::new();
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(segment);
        prefixes.push(current.clone());
    }
    prefixes
}

fn is_retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 409 | 425 | 429 | 500..=599)
}

fn retry_delay(retry: usize) -> Duration {
    Duration::from_millis(RETRY_BASE_DELAY_MS * (1_u64 << retry.min(4)))
}

fn ensure_success(resp: Response, op: &str, rel: &str) -> Result<()> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    bail!(
        "{op}失败 {rel}：HTTP {}{}",
        status.as_u16(),
        http_hint(&status)
    );
}

fn child_text<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    // 属性嵌套在 propstat/prop 下，需要在后代里查找
    node.descendants()
        .find(|n| n.tag_name().name() == name)
        .and_then(|n| n.text())
}

fn parse_multistatus(xml: &str, base_path: &str) -> Result<Vec<Entry>> {
    let doc = roxmltree::Document::parse(xml).context("解析 WebDAV 响应失败")?;
    let mut entries = Vec::new();
    for node in doc
        .descendants()
        .filter(|n| n.tag_name().name() == "response")
    {
        let Some(href) = child_text(node, "href") else {
            continue;
        };
        let href = percent_decode_str(href.split('?').next().unwrap_or("")).decode_utf8_lossy();
        let path = match Url::parse(href.trim()) {
            Ok(url) => url.path().to_string(),
            Err(_) => href.trim().to_string(),
        };
        let Some(rel) = path.strip_prefix(base_path) else {
            continue;
        };
        let rel = rel.trim_matches('/');
        if rel.is_empty() {
            continue; // 根目录自身
        }
        let is_dir = node
            .descendants()
            .any(|n| n.tag_name().name() == "collection");
        let mtime = child_text(node, "getlastmodified")
            .and_then(|t| parse_http_date(t.trim()).ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let size = child_text(node, "getcontentlength")
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0);
        entries.push(Entry {
            path: rel.to_string(),
            mtime,
            size,
            is_dir,
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response><D:href>/dav/repo/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat></D:response>
  <D:response><D:href>/dav/repo/sub/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat></D:response>
  <D:response><D:href>/dav/repo/a%20b.txt</D:href><D:propstat><D:prop><D:resourcetype/><D:getlastmodified>Sun, 20 Sep 2026 10:00:00 GMT</D:getlastmodified><D:getcontentlength>5</D:getcontentlength></D:prop></D:propstat></D:response>
</D:multistatus>"#;

    #[test]
    fn root_path_prefixes_are_ordered() {
        assert_eq!(
            path_prefixes("/dav/test"),
            vec!["dav".to_string(), "dav/test".to_string()]
        );
    }

    #[test]
    fn transient_statuses_are_retryable() {
        assert!(is_retryable_status(StatusCode::CONFLICT));
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(StatusCode::NOT_FOUND));
    }

    #[test]
    fn put_retries_explicit_transient_failures() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for status in [500, 500, 201] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 1024];
                let size = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..size]);
                assert!(request.starts_with("PUT /file.txt HTTP/1.1"));
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .unwrap();
            }
        });

        let attempts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = Arc::clone(&attempts);
        let dav = WebDav::new(&format!("http://127.0.0.1:{port}"), "", "", "").unwrap();
        assert_eq!(
            dav.put_with_progress("file.txt", b"data", move |attempt, bytes, _| {
                if bytes == 0 {
                    observed.lock().unwrap().push(attempt);
                }
            })
            .unwrap(),
            (3, 201)
        );
        assert_eq!(*attempts.lock().unwrap(), vec![1, 2, 3]);
        server.join().unwrap();
    }

    #[test]
    fn successful_put_is_not_retried() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let size = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.starts_with("PUT /file.txt HTTP/1.1"));
            stream
                .write_all(
                    b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });

        let dav = WebDav::new(&format!("http://127.0.0.1:{port}"), "", "", "").unwrap();
        assert_eq!(
            dav.put_with_progress("file.txt", b"data", |_, _, _| {})
                .unwrap(),
            (1, 201)
        );
        server.join().unwrap();
    }

    #[test]
    fn root_urls_start_at_endpoint() {
        let dav = WebDav::new("https://host/dav", "/dav/test", "", "").unwrap();
        assert_eq!(dav.root_url_for("dav").path(), "/dav/dav");
        assert_eq!(dav.root_url_for("dav/test").path(), "/dav/dav/test");
        assert_eq!(
            dav.url_for("folder/file.txt").path(),
            "/dav/dav/test/folder/file.txt"
        );
    }

    #[test]
    fn parses_http_date() {
        // httpdate 会校验星期几与日期一致（2026-09-20 是周日）
        assert!(parse_http_date("Sun, 20 Sep 2026 10:00:00 GMT").is_ok());
        assert!(parse_http_date("Wed, 20 Sep 2026 10:00:00 GMT").is_err());
    }

    #[test]
    fn parses_multistatus() {
        let entries = parse_multistatus(SAMPLE, "/dav/repo").unwrap();
        assert_eq!(entries.len(), 2);
        let dir = entries.iter().find(|e| e.is_dir).unwrap();
        assert_eq!(dir.path, "sub");
        let file = entries.iter().find(|e| !e.is_dir).unwrap();
        assert_eq!(file.path, "a b.txt");
        assert_eq!(file.size, 5);
        assert!(file.mtime > 0);
    }
}
