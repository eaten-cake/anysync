use super::{Backend, Entry, Progress, parse_endpoint, webdav::WebDav};
use anyhow::Result;
use reqwest::Url;

pub struct Alist {
    dav: WebDav,
}

pub(super) fn endpoint(address: &str) -> Result<Url> {
    let mut url = parse_endpoint(address)?;
    url.set_path("/dav");
    Ok(url)
}

impl Alist {
    pub fn new(address: &str, root: &str, username: &str, password: &str) -> Result<Self> {
        Ok(Self {
            dav: WebDav::new(endpoint(address)?.as_str(), root, username, password)?,
        })
    }
}

impl Backend for Alist {
    fn list_recursive(&self) -> Result<Vec<Entry>> {
        self.dav.list_recursive()
    }

    fn download(&self, path: &str) -> Result<Vec<u8>> {
        self.dav.download(path)
    }

    fn upload(&self, path: &str, data: &[u8], progress: Progress) -> Result<usize> {
        self.dav.upload(path, data, progress)
    }

    fn ensure_root(&self) -> Result<()> {
        self.dav.ensure_root()
    }

    fn create_dir(&self, path: &str) -> Result<()> {
        self.dav.create_dir(path)
    }
}
