use crate::platform::Platform;
use std::path::PathBuf;

/// Result of expanding a user-supplied path string for the current platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathState {
    /// Variable(s) resolved; may or may not exist on disk.
    Resolved { path: PathBuf },
    /// A variable on the *current* platform is not defined.
    UndefinedVar { var: String },
    /// The syntax belongs to another platform (e.g. `%APPDATA%` on Linux).
    OtherPlatform { raw: String },
}

impl PathState {
    pub fn resolved_path(&self) -> Option<&PathBuf> {
        match self {
            PathState::Resolved { path } => Some(path),
            _ => None,
        }
    }
}

/// Resolve `raw` for `platform`. Relative paths are resolved against `base_dir`
/// when provided, otherwise against the current working directory.
pub fn expand_path(raw: &str, platform: Platform, base_dir: Option<&std::path::Path>) -> PathState {
    let raw = raw.trim();
    if raw.is_empty() {
        return PathState::Resolved {
            path: PathBuf::from(""),
        };
    }

    let (expanded, err) = match platform {
        Platform::Windows => expand_windows(raw),
        Platform::Linux | Platform::Macos => expand_unix(raw),
    };

    match err {
        Some(ExpandErr::Undefined { var }) => PathState::UndefinedVar { var },
        Some(ExpandErr::OtherPlatform) => PathState::OtherPlatform {
            raw: raw.to_string(),
        },
        None => {
            let path = PathBuf::from(&expanded);
            let path = if path.is_absolute() {
                path
            } else {
                let base = base_dir.map(|b| b.to_path_buf()).unwrap_or_else(|| {
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                });
                base.join(&path)
            };
            PathState::Resolved { path }
        }
    }
}

enum ExpandErr {
    Undefined { var: String },
    OtherPlatform,
}

fn expand_windows(raw: &str) -> (String, Option<ExpandErr>) {
    let mut out = String::new();
    let mut rest = raw;

    // A `$VAR` or `~` appearance means Linux/macOS syntax on Windows.
    if raw.contains('$') || raw.contains("~/") || raw == "~" {
        return (raw.to_string(), Some(ExpandErr::OtherPlatform));
    }

    while let Some(pct) = rest.find('%') {
        out.push_str(&rest[..pct]);
        let after = &rest[pct + 1..];
        if let Some(closing) = after.find('%') {
            let var = &after[..closing];
            if var.is_empty() {
                out.push('%');
                rest = after;
                continue;
            }
            match std::env::var(var) {
                Ok(value) => {
                    out.push_str(&value);
                    rest = &after[closing + 1..];
                }
                Err(_) => {
                    return (
                        out,
                        Some(ExpandErr::Undefined {
                            var: var.to_string(),
                        }),
                    )
                }
            }
        } else {
            // Lone '%' at end: literal.
            out.push('%');
            rest = after;
        }
    }
    out.push_str(rest);
    (out, None)
}

fn expand_unix(raw: &str) -> (String, Option<ExpandErr>) {
    if raw.contains('%') {
        return (raw.to_string(), Some(ExpandErr::OtherPlatform));
    }

    let mut out = String::new();
    let mut rest = raw;

    // Expand ~/ and lone ~ using $HOME (resolve immediately so the value flows
    // through the rest of the pipeline unchanged).
    if rest == "~" {
        match std::env::var("HOME") {
            Ok(home) => return (home, None),
            Err(_) => {
                return (
                    String::new(),
                    Some(ExpandErr::Undefined { var: "HOME".into() }),
                )
            }
        }
    } else if rest.starts_with("~/") {
        match std::env::var("HOME") {
            Ok(home) => {
                out.push_str(&home);
                rest = &rest[1..];
            }
            Err(_) => return (out, Some(ExpandErr::Undefined { var: "HOME".into() })),
        }
    }

    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        let after = &rest[dollar + 1..];

        if after.starts_with('{') {
            // ${NAME}
            if let Some(end) = after.find('}') {
                let var = &after[1..end];
                match std::env::var(var) {
                    Ok(value) => {
                        out.push_str(&value);
                        rest = &after[end + 1..];
                    }
                    Err(_) => {
                        return (
                            out,
                            Some(ExpandErr::Undefined {
                                var: var.to_string(),
                            }),
                        )
                    }
                }
            } else {
                out.push('$');
                rest = after;
            }
        } else {
            // $NAME — name chars: alphanumeric and underscore
            let name_len = after
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(after.len());
            if name_len == 0 {
                out.push('$');
                rest = after;
                continue;
            }
            let var = &after[..name_len];
            match std::env::var(var) {
                Ok(value) => {
                    out.push_str(&value);
                    rest = &after[name_len..];
                }
                Err(_) => {
                    return (
                        out,
                        Some(ExpandErr::Undefined {
                            var: var.to_string(),
                        }),
                    )
                }
            }
        }
    }
    out.push_str(rest);
    (out, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_expand_appdata() {
        let val = "C:\\Users\\Test\\AppData\\Roaming";
        unsafe {
            std::env::set_var("APPDATA", val);
        }
        let state = expand_path("%APPDATA%\\Code\\User", Platform::Windows, None);
        match state {
            PathState::Resolved { path } => {
                assert_eq!(
                    path,
                    PathBuf::from("C:\\Users\\Test\\AppData\\Roaming\\Code\\User")
                )
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn windows_missing_var() {
        let state = expand_path("%NO_SUCH_VAR_XYZ%\\a", Platform::Windows, None);
        assert!(matches!(state, PathState::UndefinedVar { .. }));
    }

    #[test]
    fn windows_rejects_unix_syntax() {
        let state = expand_path("$HOME/.ssh", Platform::Windows, None);
        assert!(matches!(state, PathState::OtherPlatform { .. }));
    }

    #[test]
    fn unix_expand_home() {
        let home = std::env::temp_dir().join("fakehome");
        unsafe {
            std::env::set_var("HOME", &home);
        }
        let state = expand_path("~/config", Platform::Linux, None);
        match state {
            PathState::Resolved { path } => assert_eq!(path, home.join("config")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unix_expand_braced() {
        let home = std::env::temp_dir().join("fakehome");
        unsafe {
            std::env::set_var("HOME", &home);
        }
        let state = expand_path("${HOME}/x", Platform::Linux, None);
        match state {
            PathState::Resolved { path } => assert_eq!(path, home.join("x")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unix_missing_var() {
        let state = expand_path("$NO_SUCH_VAR_XYZ_2/x", Platform::Linux, None);
        assert!(matches!(state, PathState::UndefinedVar { var } if var == "NO_SUCH_VAR_XYZ_2"));
    }

    #[test]
    fn unix_rejects_windows_syntax() {
        let state = expand_path("%APPDATA%\\x", Platform::Linux, None);
        assert!(matches!(state, PathState::OtherPlatform { .. }));
    }

    #[test]
    fn relative_resolves_against_base() {
        let base = PathBuf::from("/tmp/base");
        let state = expand_path("rel/dir", Platform::Linux, Some(&base));
        match state {
            PathState::Resolved { path } => assert_eq!(path, base.join("rel/dir")),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
