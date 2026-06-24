use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};

pub trait HostPaths {
    fn state_root(&self) -> PathBuf;
    fn logs_dir(&self) -> PathBuf {
        self.state_root().join("logs")
    }
    fn default_mount_root(&self) -> PathBuf;
    fn user_home(&self) -> Option<PathBuf>;
}

#[derive(Clone, Debug)]
pub struct DefaultHostPaths {
    target_os: String,
    env: BTreeMap<String, OsString>,
}

impl DefaultHostPaths {
    pub fn current() -> Self {
        Self::for_target(std::env::consts::OS)
    }

    pub fn for_target(target_os: impl Into<String>) -> Self {
        Self {
            target_os: target_os.into(),
            env: std::env::vars_os()
                .map(|(key, value)| (key.to_string_lossy().to_string(), value))
                .collect(),
        }
    }

    pub fn for_target_with_env<K, V>(
        target_os: impl Into<String>,
        vars: impl IntoIterator<Item = (K, V)>,
    ) -> Self
    where
        K: Into<String>,
        V: Into<OsString>,
    {
        Self {
            target_os: target_os.into(),
            env: vars
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        }
    }

    fn var_path(&self, key: &str) -> Option<PathBuf> {
        self.env
            .get(key)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }
}

impl HostPaths for DefaultHostPaths {
    fn state_root(&self) -> PathBuf {
        if let Some(path) = self.var_path("LOCALITY_STATE_DIR") {
            return path;
        }

        if self.target_os == "windows" {
            if let Some(path) = self.var_path("LOCALAPPDATA") {
                return path.join("Locality");
            }
            if let Some(path) = self.var_path("USERPROFILE") {
                return path.join("AppData").join("Local").join("Locality");
            }
        }

        self.user_home()
            .map(|home| home.join(".loc"))
            .unwrap_or_else(|| PathBuf::from(".loc"))
    }

    fn default_mount_root(&self) -> PathBuf {
        self.user_home()
            .map(|home| home.join("Locality"))
            .unwrap_or_else(|| PathBuf::from("Locality"))
    }

    fn user_home(&self) -> Option<PathBuf> {
        if self.target_os == "windows" {
            return self
                .var_path("USERPROFILE")
                .or_else(|| self.var_path("HOME"))
                .or_else(|| {
                    let drive = self.env.get("HOMEDRIVE")?;
                    let path = self.env.get("HOMEPATH")?;
                    let mut combined = OsString::from(drive);
                    combined.push(path);
                    Some(PathBuf::from(combined))
                });
        }

        self.var_path("HOME")
    }
}

pub enum ReportPath<'a> {
    Logical(&'a Path),
    Host(&'a Path),
}

impl Display for ReportPath<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Logical(path) => f.write_str(&logical_path_display(path)),
            Self::Host(path) => write!(f, "{}", path.display()),
        }
    }
}

pub fn default_state_root() -> PathBuf {
    DefaultHostPaths::current().state_root()
}

pub fn default_mount_root() -> PathBuf {
    DefaultHostPaths::current().default_mount_root()
}

pub fn user_home() -> Option<PathBuf> {
    DefaultHostPaths::current().user_home()
}

pub fn logical_path_display(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

pub fn host_path_from_logical_path(path: &Path) -> PathBuf {
    let logical = logical_path_display(path);
    let mut host_path = PathBuf::new();
    for component in logical.split('/') {
        match component {
            "" | "." => {}
            ".." => host_path.push(".."),
            component => host_path.push(component),
        }
    }
    host_path
}

pub fn join_logical_path(root: &Path, logical_path: &Path) -> PathBuf {
    let host_relative_path = host_path_from_logical_path(logical_path);
    if host_relative_path.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(host_relative_path)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DefaultHostPaths, HostPaths, host_path_from_logical_path, join_logical_path,
        logical_path_display,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn windows_state_root_uses_local_app_data() {
        let paths = DefaultHostPaths::for_target_with_env(
            "windows",
            [("LOCALAPPDATA", r"C:\Users\Ada\AppData\Local")],
        );

        assert_eq!(
            paths.state_root(),
            PathBuf::from(r"C:\Users\Ada\AppData\Local").join("Locality")
        );
    }

    #[test]
    fn unix_state_root_preserves_home_dot_loc() {
        let paths = DefaultHostPaths::for_target_with_env("linux", [("HOME", "/home/ada")]);

        assert_eq!(paths.state_root(), PathBuf::from("/home/ada/.loc"));
    }

    #[test]
    fn env_state_root_overrides_platform_default() {
        let paths = DefaultHostPaths::for_target_with_env(
            "windows",
            [
                ("LOCALITY_STATE_DIR", r"D:\loc-state"),
                ("LOCALAPPDATA", r"C:\Users\Ada\AppData\Local"),
            ],
        );

        assert_eq!(paths.state_root(), PathBuf::from(r"D:\loc-state"));
    }

    #[test]
    fn logical_paths_render_with_forward_slashes() {
        assert_eq!(
            logical_path_display(Path::new(r"Teamspace Home\Launch Plan\page.md")),
            "Teamspace Home/Launch Plan/page.md"
        );
    }

    #[test]
    fn logical_paths_convert_to_host_relative_paths() {
        assert_eq!(
            host_path_from_logical_path(Path::new(r"Teamspace Home\Launch Plan/page.md")),
            PathBuf::from("Teamspace Home")
                .join("Launch Plan")
                .join("page.md")
        );
    }

    #[test]
    fn logical_paths_join_to_host_roots_with_host_separators() {
        assert_eq!(
            join_logical_path(
                Path::new("mount"),
                Path::new("Teamspace Home/Launch Plan/page.md")
            ),
            PathBuf::from("mount")
                .join("Teamspace Home")
                .join("Launch Plan")
                .join("page.md")
        );
    }
}
