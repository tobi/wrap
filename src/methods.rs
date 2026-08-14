//! Fast in-VM file methods for an already-running wrap.
//!
//! Selectors match omp/pi: `file`, `file:5`, `file:5-10`, `file:5:2`.

use anyhow::{Context, Result};
use microsandbox::Sandbox;

use crate::{WORKSPACE, shell_quote};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineSpec {
    All,
    From(usize),
    Range { start: usize, end: usize },
    Count { start: usize, count: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    pub path: String,
    pub lines: LineSpec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    Ls {
        args: Vec<String>,
    },
    Grep {
        pattern: String,
        args: Vec<String>,
    },
    Read {
        selector: Selector,
    },
    Write {
        path: String,
        content: Option<String>,
    },
    Bash {
        script: String,
    },
    Find {
        args: Vec<String>,
    },
}

impl Method {
    pub fn parse(command: &[String]) -> Option<Self> {
        let (name, rest) = command.split_first()?;
        match name.as_str() {
            "ls" => Some(Self::Ls {
                args: rest.to_vec(),
            }),
            "grep" => {
                let (pattern, rest) = rest.split_first()?;
                Some(Self::Grep {
                    pattern: pattern.clone(),
                    args: rest.to_vec(),
                })
            }
            "read" => Some(Self::Read {
                selector: parse_selector(rest.first()?.as_str()),
            }),
            "write" => {
                let (path, rest) = rest.split_first()?;
                let content = if rest.is_empty() {
                    None
                } else {
                    Some(rest.join(" "))
                };
                Some(Self::Write {
                    path: guest_path(path),
                    content,
                })
            }
            "bash" => {
                if rest.is_empty() {
                    return None;
                }
                Some(Self::Bash {
                    script: rest.join(" "),
                })
            }
            "find" => Some(Self::Find {
                args: rest.to_vec(),
            }),
            _ => None,
        }
    }
}

pub fn parse_selector(raw: &str) -> Selector {
    if let Some((path, start, count)) = split_count(raw) {
        return Selector {
            path: guest_path(path),
            lines: LineSpec::Count { start, count },
        };
    }
    if let Some((path, start, end)) = split_range(raw) {
        return Selector {
            path: guest_path(path),
            lines: LineSpec::Range { start, end },
        };
    }
    if let Some((path, start)) = split_from(raw) {
        return Selector {
            path: guest_path(path),
            lines: LineSpec::From(start),
        };
    }
    Selector {
        path: guest_path(raw),
        lines: LineSpec::All,
    }
}

fn split_count(raw: &str) -> Option<(&str, usize, usize)> {
    let (left, count) = raw.rsplit_once(':')?;
    let count = parse_line(count)?;
    let (path, start) = left.rsplit_once(':')?;
    let start = parse_line(start)?;
    if path.is_empty() {
        return None;
    }
    Some((path, start, count))
}

fn split_range(raw: &str) -> Option<(&str, usize, usize)> {
    let (path, span) = raw.rsplit_once(':')?;
    let (start, end) = span.split_once('-')?;
    let start = parse_line(start)?;
    let end = parse_line(end)?;
    if path.is_empty() || start > end {
        return None;
    }
    Some((path, start, end))
}

fn split_from(raw: &str) -> Option<(&str, usize)> {
    let (path, start) = raw.rsplit_once(':')?;
    let start = parse_line(start)?;
    if path.is_empty() || start == 0 {
        return None;
    }
    Some((path, start))
}

fn parse_line(raw: &str) -> Option<usize> {
    let n: usize = raw.parse().ok()?;
    (n >= 1).then_some(n)
}

pub fn guest_path(raw: &str) -> String {
    if raw.is_empty() || raw == "." {
        return WORKSPACE.into();
    }
    if raw.starts_with('/') {
        return raw.to_string();
    }
    format!("{WORKSPACE}/{raw}")
}

pub async fn run_method(sandbox: &Sandbox, method: Method) -> Result<u8> {
    match method {
        Method::Ls { args } => exec_sh(sandbox, &ls_script(&args)).await,
        Method::Grep { pattern, args } => exec_sh(sandbox, &grep_script(&pattern, &args)).await,
        Method::Read { selector } => exec_sh(sandbox, &read_script(&selector)).await,
        Method::Write { path, content } => write_guest(sandbox, &path, content).await,
        Method::Bash { script } => exec_sh(sandbox, &bash_script(&script)).await,
        Method::Find { args } => exec_sh(sandbox, &find_script(&args)).await,
    }
}

fn ls_script(args: &[String]) -> String {
    if args.is_empty() {
        return format!("ls -la -- {}", shell_quote(WORKSPACE));
    }
    if args.iter().all(|a| a.starts_with('-')) {
        return format!(
            "ls {} -- {}",
            shell_join_local(args),
            shell_quote(WORKSPACE)
        );
    }
    let mut out = Vec::new();
    for arg in args {
        if arg.starts_with('-') {
            out.push(arg.clone());
        } else {
            out.push(guest_path(arg));
        }
    }
    format!("ls -la -- {}", shell_join_local(&out))
}

fn grep_script(pattern: &str, args: &[String]) -> String {
    let path = if args.is_empty() {
        WORKSPACE.to_string()
    } else {
        guest_path(&args[0])
    };
    let extra = if args.len() > 1 {
        format!(" {}", shell_join_local(&args[1..]))
    } else {
        String::new()
    };
    format!(
        "grep -n -R -- {} {}{}",
        shell_quote(pattern),
        shell_quote(&path),
        extra
    )
}

fn read_script(selector: &Selector) -> String {
    let path = shell_quote(&selector.path);
    match selector.lines {
        LineSpec::All => format!("cat -- {path}"),
        LineSpec::From(start) => format!("tail -n +{start} -- {path}"),
        LineSpec::Range { start, end } => format!("sed -n '{start},{end}p' -- {path}"),
        LineSpec::Count { start, count } => {
            let end = start.saturating_add(count.saturating_sub(1));
            format!("sed -n '{start},{end}p' -- {path}")
        }
    }
}

fn find_script(args: &[String]) -> String {
    if args.is_empty() {
        return format!("find {}", shell_quote(WORKSPACE));
    }
    if args[0].starts_with('-') {
        return format!("find {} {}", shell_quote(WORKSPACE), shell_join_local(args));
    }
    let path = guest_path(&args[0]);
    if args.len() == 1 {
        return format!("find {}", shell_quote(&path));
    }
    format!(
        "find {} {}",
        shell_quote(&path),
        shell_join_local(&args[1..])
    )
}

fn bash_script(script: &str) -> String {
    format!(
        r#"export PATH="/root/.local/bin:/root/.local/share/mise/shims:$PATH"; {}"#,
        script
    )
}

fn shell_join_local(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

async fn exec_sh(sandbox: &Sandbox, script: &str) -> Result<u8> {
    let output = sandbox
        .exec_with("/bin/sh", |e| {
            e.args(["-c", script])
                .cwd(WORKSPACE)
                .timeout(std::time::Duration::from_secs(30))
        })
        .await
        .context("wrap method")?;
    std::io::Write::write_all(&mut std::io::stdout(), output.stdout_bytes())?;
    std::io::Write::write_all(&mut std::io::stderr(), output.stderr_bytes())?;
    Ok(output.status().code as u8)
}

async fn write_guest(sandbox: &Sandbox, path: &str, content: Option<String>) -> Result<u8> {
    let bytes = match content {
        Some(text) => text.into_bytes(),
        None => {
            use std::io::Read;
            let mut buf = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buf)
                .context("read stdin for wrap write")?;
            buf
        }
    };
    let fs = sandbox.fs();
    if let Some(parent) = std::path::Path::new(path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs.mkdir(&parent.to_string_lossy())
            .await
            .context("create wrap write parent")?;
    }
    fs.write(path, bytes).await.context("wrap write")?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_omp_selectors() {
        assert_eq!(
            parse_selector("src/main.rs:5:2"),
            Selector {
                path: "/workspace/src/main.rs".into(),
                lines: LineSpec::Count { start: 5, count: 2 },
            }
        );
        assert_eq!(
            parse_selector("/workspace/foo.rs:10-12"),
            Selector {
                path: "/workspace/foo.rs".into(),
                lines: LineSpec::Range { start: 10, end: 12 },
            }
        );
        assert_eq!(
            parse_selector("README.md:3"),
            Selector {
                path: "/workspace/README.md".into(),
                lines: LineSpec::From(3),
            }
        );
        assert_eq!(parse_selector(".").path, "/workspace");
    }

    #[test]
    fn read_script_uses_sed_count() {
        let sel = parse_selector("file:5:2");
        assert_eq!(read_script(&sel), "sed -n '5,6p' -- /workspace/file");
    }

    #[test]
    fn parses_methods() {
        assert!(matches!(
            Method::parse(&["ls".into()]),
            Some(Method::Ls { .. })
        ));
        assert!(Method::parse(&["grep".into()]).is_none());
        assert!(matches!(
            Method::parse(&["grep".into(), "foo".into(), "src".into()]),
            Some(Method::Grep { .. })
        ));
        assert!(Method::parse(&["omp".into()]).is_none());
        assert!(Method::parse(&["bash".into()]).is_none());
        assert!(matches!(
            Method::parse(&["bash".into(), "uname".into(), "-a".into()]),
            Some(Method::Bash { script }) if script == "uname -a"
        ));
    }
}
