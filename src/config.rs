use crate::backend;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub remote: Remote,
}

#[derive(Serialize, Deserialize, clap::ValueEnum, Default, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Alist,
    // 未指定后端的旧配置沿用原有 endpoint。
    #[default]
    Webdav,
}

impl Backend {
    fn as_str(self) -> &'static str {
        match self {
            Self::Alist => "alist",
            Self::Webdav => "webdav",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "alist" => Ok(Self::Alist),
            "webdav" => Ok(Self::Webdav),
            _ => bail!("不支持的存储后端：{value}，请选择 alist 或 webdav"),
        }
    }
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Remote {
    #[serde(default)]
    pub backend: Backend,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
}

impl Remote {
    pub fn endpoint(&self) -> Result<String> {
        backend::endpoint(self.backend, &self.url)
    }
}

/// 从当前目录向上查找 .anysync 目录，类似 git。
pub fn repo_dir() -> Result<PathBuf> {
    let mut dir = std::env::current_dir().context("无法获取当前目录")?;
    loop {
        let dot = dir.join(".anysync");
        if dot.is_dir() {
            return Ok(dot);
        }
        if !dir.pop() {
            bail!("当前目录不在 anysync 仓库内，请先运行 anysync init");
        }
    }
}

pub fn init() -> Result<()> {
    let dot = PathBuf::from(".anysync");
    if dot.join("config.toml").exists() {
        println!("anysync 仓库已存在");
        return Ok(());
    }
    fs::create_dir_all(&dot)?;
    save(&dot, &Config::default())?;
    println!(
        "已初始化 anysync 仓库：{}",
        dot.join("config.toml").display()
    );
    Ok(())
}

pub fn load(dot: &Path) -> Result<Config> {
    let text = fs::read_to_string(dot.join("config.toml")).context("读取配置失败")?;
    toml::from_str(&text).context("解析配置失败")
}

fn save(dot: &Path, cfg: &Config) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(dot)?;
    write!(file, "{}", toml::to_string_pretty(cfg)?)?;
    file.persist(dot.join("config.toml"))
        .map_err(|err| err.error)?;
    Ok(())
}

/// 指定参数时只更新对应项，无参数时逐项交互询问。
pub fn remote_config(
    backend: Option<Backend>,
    url: Option<String>,
    root: Option<String>,
    username: Option<String>,
    ask_password: bool,
) -> Result<()> {
    let dot = repo_dir()?;
    let mut remote = load(&dot)?.remote;
    let interactive =
        backend.is_none() && url.is_none() && root.is_none() && username.is_none() && !ask_password;

    if interactive {
        remote.backend =
            Backend::parse(&ask("存储后端（alist / webdav）", remote.backend.as_str())?)?;
        let label = match remote.backend {
            Backend::Alist => "AList 服务地址（如 https://host，自动使用 /dav）",
            Backend::Webdav => "WebDAV endpoint（如 https://host/remote.php/dav/files/user）",
        };
        remote.url = ask(label, &remote.url)?;
    } else {
        if let Some(v) = backend {
            remote.backend = v;
        }
        if let Some(v) = url {
            remote.url = v;
        }
    }
    if !remote.url.is_empty() {
        remote.url = remote.endpoint()?;
    } else if interactive {
        bail!("远端地址不能为空");
    }
    if interactive {
        println!("WebDAV endpoint：{}", remote.url);
        remote.root = ask(
            "远端根目录（相对于 endpoint，如 /test；/ 表示根目录）",
            &remote.root,
        )?;
        remote.username = ask("用户名", &remote.username)?;
    } else {
        if let Some(v) = root {
            remote.root = v;
        }
        if let Some(v) = username {
            remote.username = v;
        }
    }
    if interactive || ask_password {
        let password = prompt_password("密码（回车保持不变）: ")?;
        if !password.is_empty() {
            remote.password = password;
        }
    }
    save(&dot, &Config { remote })?;
    println!("配置已保存：{}", dot.join("config.toml").display());
    Ok(())
}

/// 交互终端下无回显读取；非交互环境从 stdin 读一行。
fn prompt_password(label: &str) -> Result<String> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        // rpassword 自带的提示输出在中文 Windows 下会乱码，改由标准输出打印
        print!("{label}");
        io::stdout().flush()?;
        Ok(rpassword::read_password()?)
    } else {
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        Ok(line.trim().to_string())
    }
}

fn ask(label: &str, current: &str) -> Result<String> {
    match current {
        "" => print!("{label}: "),
        _ => print!("{label} [{current}]: "),
    }
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let value = line.trim();
    Ok(if value.is_empty() {
        current.to_string()
    } else {
        value.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alist_uses_origin_and_dav_path() {
        for address in [
            "https://host:8443",
            "https://host:8443/",
            "https://host:8443/dav/",
            "https://host:8443/some/path",
        ] {
            let remote = Remote {
                backend: Backend::Alist,
                url: address.into(),
                root: "/test".into(),
                ..Remote::default()
            };
            assert_eq!(remote.endpoint().unwrap(), "https://host:8443/dav");
            assert_eq!(remote.root, "/test");
        }
    }

    #[test]
    fn webdav_preserves_custom_endpoint() {
        let remote = Remote {
            url: "https://host/remote.php/dav/files/user/".into(),
            ..Remote::default()
        };
        assert_eq!(remote.endpoint().unwrap(), remote.url);
    }

    #[test]
    fn legacy_config_defaults_to_webdav() {
        let cfg: Config =
            toml::from_str("[remote]\nurl = 'https://host/custom'\nroot = '/dav/test'").unwrap();
        assert_eq!(cfg.remote.backend, Backend::Webdav);
        assert_eq!(cfg.remote.endpoint().unwrap(), "https://host/custom");
        assert_eq!(cfg.remote.root, "/dav/test");
    }

    #[test]
    fn backend_config_roundtrips_and_rejects_unknown_values() {
        let cfg: Config =
            toml::from_str("[remote]\nbackend = 'alist'\nurl = 'http://host'").unwrap();
        let saved = toml::to_string(&cfg).unwrap();
        let loaded: Config = toml::from_str(&saved).unwrap();
        assert_eq!(loaded.remote.backend, Backend::Alist);
        assert_eq!(loaded.remote.endpoint().unwrap(), "http://host/dav");
        assert!(toml::from_str::<Config>("[remote]\nbackend = 'unknown'").is_err());
        assert!(Backend::parse("unknown").is_err());
    }

    #[test]
    fn endpoint_rejects_invalid_or_embedded_auth_urls() {
        for backend in [Backend::Alist, Backend::Webdav] {
            for address in [
                "host",
                "ftp://host/path",
                "https://user:secret@host/path",
                "https://host/path?token=secret",
                "https://host/path#fragment",
            ] {
                let remote = Remote {
                    backend,
                    url: address.into(),
                    ..Remote::default()
                };
                assert!(remote.endpoint().is_err());
            }
        }
    }
}
