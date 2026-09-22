mod alist;
mod webdav;

use crate::config::{Backend as BackendKind, Remote};
use anyhow::{Context, Result, bail};
use reqwest::Url;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct Entry {
    /// 相对远端根目录的路径，以 / 分隔
    pub path: String,
    pub mtime: i64,
    pub size: u64,
    pub is_dir: bool,
}

/// 参数依次为尝试次数、已读取字节数、总字节数；不代表服务端已持久化。
pub type Progress = Arc<dyn Fn(usize, u64, u64) + Send + Sync>;

pub trait Backend {
    fn list_recursive(&self) -> Result<Vec<Entry>>;
    fn download(&self, path: &str) -> Result<Vec<u8>>;
    /// 返回成功时的尝试次数；上传结果不确定时返回错误，不因列表暂不可见而重传。
    fn upload(&self, path: &str, data: &[u8], progress: Progress) -> Result<usize>;
    fn ensure_root(&self) -> Result<()>;
    fn create_dir(&self, path: &str) -> Result<()>;
}

pub fn connect(remote: &Remote, password: &str) -> Result<Box<dyn Backend>> {
    match remote.backend {
        BackendKind::Alist => Ok(Box::new(alist::Alist::new(
            &remote.url,
            &remote.root,
            &remote.username,
            password,
        )?)),
        BackendKind::Webdav => Ok(Box::new(webdav::WebDav::new(
            &remote.url,
            &remote.root,
            &remote.username,
            password,
        )?)),
    }
}

pub fn endpoint(kind: BackendKind, address: &str) -> Result<String> {
    let url = match kind {
        BackendKind::Alist => alist::endpoint(address)?,
        BackendKind::Webdav => parse_endpoint(address)?,
    };
    Ok(url.to_string())
}

fn parse_endpoint(address: &str) -> Result<Url> {
    // 不把原始地址放进错误消息，避免泄露误填在 URL 中的凭据。
    let url = Url::parse(address).context("远端地址无效，请输入完整的 http/https 地址")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("远端地址必须是完整的 http/https 地址");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("远端地址不能包含用户名或密码，请单独配置认证信息");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("远端地址不能包含 query 或 fragment");
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::time::Duration;

    #[test]
    fn configured_backends_transfer_through_common_interface() {
        for (kind, prefix) in [
            (BackendKind::Alist, "/dav"),
            (BackendKind::Webdav, "/custom"),
        ] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let xml = format!(
                    "<multistatus xmlns=\"DAV:\"><response><href>{prefix}/repo/file.txt</href>\
                     <propstat><prop><resourcetype/><getcontentlength>4</getcontentlength>\
                     <getlastmodified>Sun, 20 Sep 2026 10:00:00 GMT</getlastmodified>\
                     </prop></propstat></response></multistatus>"
                );
                for (method, suffix, status, body) in [
                    ("MKCOL", "/repo", 405, ""),
                    ("MKCOL", "/repo/sub", 201, ""),
                    ("PROPFIND", "/repo", 207, xml.as_str()),
                    ("GET", "/repo/file.txt", 200, "data"),
                    ("PUT", "/repo/file.txt", 201, ""),
                ] {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut request = BufReader::new(&mut stream);
                    let mut line = String::new();
                    request.read_line(&mut line).unwrap();
                    assert_eq!(line.trim(), format!("{method} {prefix}{suffix} HTTP/1.1"));
                    let mut length = 0;
                    loop {
                        line.clear();
                        assert!(request.read_line(&mut line).unwrap() > 0);
                        if line == "\r\n" {
                            break;
                        }
                        if let Some(value) =
                            line.to_ascii_lowercase().strip_prefix("content-length:")
                        {
                            length = value.trim().parse::<usize>().unwrap();
                        }
                    }
                    let mut data = vec![0; length];
                    request.read_exact(&mut data).unwrap();
                    if method == "PUT" {
                        assert_eq!(data, b"data");
                    }
                    write!(stream, "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }
            });
            let remote = Remote {
                backend: kind,
                url: format!("http://{address}/custom"),
                root: "/repo".into(),
                ..Remote::default()
            };
            let backend = connect(&remote, "").unwrap();
            backend.ensure_root().unwrap();
            backend.create_dir("sub").unwrap();
            let entries = backend.list_recursive().unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].path, "file.txt");
            assert_eq!(entries[0].size, 4);
            assert!(entries[0].mtime > 0);
            assert_eq!(backend.download("file.txt").unwrap(), b"data");
            let events = Arc::new(Mutex::new(Vec::new()));
            let observed = Arc::clone(&events);
            let attempts = backend
                .upload(
                    "file.txt",
                    b"data",
                    Arc::new(move |attempt, bytes, total| {
                        observed.lock().unwrap().push((attempt, bytes, total));
                    }),
                )
                .unwrap();
            assert_eq!(attempts, 1);
            let events = events.lock().unwrap();
            assert_eq!(events.first(), Some(&(1, 0, 4)));
            assert_eq!(events.last(), Some(&(1, 4, 4)));
            server.join().unwrap();
        }
    }
}
