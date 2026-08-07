use std::io;
use std::sync::Mutex;
use std::thread;

#[cfg(unix)]
use std::fs::{self, File, OpenOptions};
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::time::{Duration, Instant};
#[cfg(target_os = "macos")]
use std::{ffi::OsString, os::unix::ffi::OsStringExt};

#[cfg(windows)]
use windows_sys::Win32::Foundation::HANDLE;

#[cfg(windows)]
const WINDOWS_DESKTOP_SINGLE_INSTANCE_MUTEX: &str =
    r"Local\CodeFlash.Locality.Desktop.SingleInstance";
#[cfg(windows)]
const WINDOWS_DESKTOP_ACTIVATION_EVENT: &str = r"Local\CodeFlash.Locality.Desktop.Activate";
#[cfg(unix)]
const UNIX_ACTIVATION_RETRY_TIMEOUT: Duration = Duration::from_millis(500);
#[cfg(unix)]
const UNIX_ACTIVATION_RETRY_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(target_os = "macos")]
const DARWIN_UNIX_SOCKET_PATH_MAX: usize = 103;

pub struct DesktopSingleInstanceGuard {
    #[cfg(unix)]
    _lock_file: File,
    #[cfg(unix)]
    activation_listener: UnixListener,
    #[cfg(unix)]
    activation_socket_path: PathBuf,
    #[cfg(windows)]
    mutex_handle: usize,
    #[cfg(windows)]
    activation_event_handle: usize,
}

pub struct DesktopSingleInstanceState {
    guard: Mutex<Option<DesktopSingleInstanceGuard>>,
}

pub struct DesktopActivationReceiver {
    #[cfg(unix)]
    activation_listener: UnixListener,
    #[cfg(windows)]
    activation_event_handle: usize,
}

impl DesktopSingleInstanceState {
    pub fn new(guard: Option<DesktopSingleInstanceGuard>) -> Self {
        Self {
            guard: Mutex::new(guard),
        }
    }

    pub fn release_for_relaunch(&self) -> io::Result<bool> {
        let guard = self
            .guard
            .lock()
            .map_err(|_| io::Error::other("desktop single-instance state lock is poisoned"))?
            .take();
        let released = guard.is_some();
        drop(guard);
        Ok(released)
    }
}

impl DesktopSingleInstanceGuard {
    pub fn activation_receiver(&self) -> io::Result<DesktopActivationReceiver> {
        #[cfg(unix)]
        {
            self.activation_listener
                .try_clone()
                .map(|activation_listener| DesktopActivationReceiver {
                    activation_listener,
                })
        }

        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
            use windows_sys::Win32::System::Threading::GetCurrentProcess;

            let current_process = unsafe { GetCurrentProcess() };
            let mut activation_event_handle = std::ptr::null_mut();
            if unsafe {
                DuplicateHandle(
                    current_process,
                    self.activation_event_handle as HANDLE,
                    current_process,
                    &mut activation_event_handle,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(DesktopActivationReceiver {
                activation_event_handle: activation_event_handle as usize,
            })
        }

        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "desktop single-instance coordination is unsupported on this platform",
            ))
        }
    }
}

impl DesktopActivationReceiver {
    pub fn start<F>(self, on_activate: F)
    where
        F: Fn() + Send + 'static,
    {
        #[cfg(unix)]
        thread::spawn(move || {
            loop {
                match self.activation_listener.accept() {
                    Ok((_stream, _address)) => on_activate(),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });

        #[cfg(windows)]
        thread::spawn(move || {
            use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0};
            use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

            loop {
                let result = unsafe {
                    WaitForSingleObject(self.activation_event_handle as HANDLE, INFINITE)
                };
                if result == WAIT_OBJECT_0 {
                    on_activate();
                } else if result == WAIT_FAILED {
                    break;
                }
            }
        });

        #[cfg(not(any(unix, windows)))]
        {
            let _ = self;
            let _ = on_activate;
        }
    }
}

#[cfg(windows)]
impl Drop for DesktopActivationReceiver {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;

        unsafe {
            let _ = CloseHandle(self.activation_event_handle as HANDLE);
        }
    }
}

#[cfg(unix)]
impl Drop for DesktopSingleInstanceGuard {
    fn drop(&mut self) {
        // The lock is still held while the endpoint is removed, so a successor
        // cannot bind and then have its socket removed by this process.
        let _ = fs::remove_file(&self.activation_socket_path);
    }
}

#[cfg(windows)]
impl Drop for DesktopSingleInstanceGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;

        unsafe {
            let _ = CloseHandle(self.activation_event_handle as HANDLE);
            let _ = CloseHandle(self.mutex_handle as HANDLE);
        }
    }
}

pub fn acquire_desktop_single_instance(
    background_launch: bool,
) -> io::Result<Option<DesktopSingleInstanceGuard>> {
    #[cfg(unix)]
    {
        acquire_desktop_single_instance_at(
            &desktop_single_instance_coordination_dir()?,
            background_launch,
        )
    }

    #[cfg(windows)]
    {
        acquire_desktop_single_instance_windows(background_launch)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = background_launch;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "desktop single-instance coordination is unsupported on this platform",
        ))
    }
}

#[cfg(target_os = "macos")]
fn desktop_single_instance_coordination_dir() -> io::Result<PathBuf> {
    macos_desktop_single_instance_coordination_dir(&macos_user_temp_dir()?)
}

#[cfg(target_os = "macos")]
fn macos_user_temp_dir() -> io::Result<PathBuf> {
    let required =
        unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, std::ptr::null_mut(), 0) };
    if required == 0 {
        return Err(io::Error::other(
            "macOS did not provide a private user temporary directory",
        ));
    }

    let mut bytes = vec![0_u8; required];
    let written = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            bytes.as_mut_ptr().cast(),
            bytes.len(),
        )
    };
    if written == 0 || written > bytes.len() || bytes.get(written - 1) != Some(&0) {
        return Err(io::Error::other(
            "macOS returned an invalid private user temporary directory",
        ));
    }
    bytes.truncate(written - 1);

    let path = PathBuf::from(OsString::from_vec(bytes));
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "macOS returned a relative private user temporary directory",
        ));
    }
    Ok(path)
}

#[cfg(target_os = "macos")]
fn macos_desktop_single_instance_coordination_dir(user_temp_dir: &Path) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;

    let coordination_dir = user_temp_dir.join("locality-si");
    let activation_socket_path = coordination_dir.join("activate.sock");
    if activation_socket_path.as_os_str().as_bytes().len() > DARWIN_UNIX_SOCKET_PATH_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "desktop activation socket path exceeds the Darwin limit: {}",
                activation_socket_path.display()
            ),
        ));
    }
    Ok(coordination_dir)
}

#[cfg(target_os = "linux")]
fn desktop_single_instance_coordination_dir() -> io::Result<PathBuf> {
    linux_desktop_single_instance_coordination_dir(
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        locality_platform::user_home(),
    )
}

#[cfg(target_os = "linux")]
fn linux_desktop_single_instance_coordination_dir(
    runtime_dir: Option<PathBuf>,
    user_home: Option<PathBuf>,
) -> io::Result<PathBuf> {
    if let Some(runtime_dir) = runtime_dir.filter(|path| path.is_absolute()) {
        return Ok(runtime_dir.join("locality-desktop-si"));
    }
    let user_home = user_home
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Linux desktop single-instance coordination requires XDG_RUNTIME_DIR or an absolute user home",
            )
        })?;
    Ok(user_home.join(".locality-desktop-si"))
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn desktop_single_instance_coordination_dir() -> io::Result<PathBuf> {
    let effective_user_id = unsafe { libc::geteuid() };
    Ok(std::env::temp_dir().join(format!("locality-desktop-si-{effective_user_id}")))
}

#[cfg(unix)]
fn acquire_desktop_single_instance_at(
    coordination_dir: &Path,
    background_launch: bool,
) -> io::Result<Option<DesktopSingleInstanceGuard>> {
    ensure_private_coordination_dir(coordination_dir)?;
    let lock_file = open_coordination_lock(&coordination_dir.join("lock"))?;
    match try_lock_coordination_file(&lock_file) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            if !background_launch
                && let Err(error) = notify_existing_desktop(&coordination_dir.join("activate.sock"))
            {
                eprintln!("loc desktop could not activate the existing process: {error}");
            }
            return Ok(None);
        }
        Err(error) => return Err(error),
    }

    let activation_socket_path = coordination_dir.join("activate.sock");
    match fs::remove_file(&activation_socket_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let activation_listener = UnixListener::bind(&activation_socket_path)?;
    fs::set_permissions(&activation_socket_path, fs::Permissions::from_mode(0o600))?;

    Ok(Some(DesktopSingleInstanceGuard {
        _lock_file: lock_file,
        activation_listener,
        activation_socket_path,
    }))
}

#[cfg(unix)]
fn ensure_private_coordination_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "desktop coordination path is not a directory: {}",
                path.display()
            ),
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "desktop coordination directory is not owned by the current user: {}",
                path.display()
            ),
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(unix)]
fn open_coordination_lock(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(unix)]
fn try_lock_coordination_file(file: &File) -> io::Result<()> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Err(io::Error::from(io::ErrorKind::WouldBlock))
    } else {
        Err(error)
    }
}

#[cfg(unix)]
fn notify_existing_desktop(socket_path: &Path) -> io::Result<()> {
    let deadline = Instant::now() + UNIX_ACTIVATION_RETRY_TIMEOUT;
    loop {
        match UnixStream::connect(socket_path) {
            Ok(mut stream) => return stream.write_all(b"activate"),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) && Instant::now() < deadline =>
            {
                thread::sleep(UNIX_ACTIVATION_RETRY_INTERVAL);
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(windows)]
fn acquire_desktop_single_instance_windows(
    background_launch: bool,
) -> io::Result<Option<DesktopSingleInstanceGuard>> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::{CreateEventW, CreateMutexW, SetEvent};

    // Create/open the event first so it already exists when a competing mutex
    // claimant loses. An auto-reset event retains an early activation signal
    // until the primary process starts its receiver thread.
    let activation_event_name = windows_wide_null(WINDOWS_DESKTOP_ACTIVATION_EVENT);
    let activation_event_handle =
        unsafe { CreateEventW(std::ptr::null(), 0, 0, activation_event_name.as_ptr()) };
    if activation_event_handle.is_null() {
        return Err(io::Error::last_os_error());
    }

    let mutex_name = windows_wide_null(WINDOWS_DESKTOP_SINGLE_INSTANCE_MUTEX);
    let mutex_handle = unsafe { CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr()) };
    if mutex_handle.is_null() {
        let error = io::Error::last_os_error();
        unsafe {
            let _ = CloseHandle(activation_event_handle);
        }
        return Err(error);
    }
    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already_running {
        if !background_launch && unsafe { SetEvent(activation_event_handle) } == 0 {
            eprintln!(
                "loc desktop could not activate the existing process: {}",
                io::Error::last_os_error()
            );
        }
        unsafe {
            let _ = CloseHandle(mutex_handle);
            let _ = CloseHandle(activation_event_handle);
        }
        return Ok(None);
    }

    Ok(Some(DesktopSingleInstanceGuard {
        mutex_handle: mutex_handle as usize,
        activation_event_handle: activation_event_handle as usize,
    }))
}

#[cfg(windows)]
fn windows_wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::io::Read;
    #[cfg(unix)]
    use std::sync::atomic::{AtomicU64, Ordering};
    #[cfg(unix)]
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(unix)]
    struct TestCoordinationDir {
        path: std::path::PathBuf,
    }

    #[cfg(unix)]
    impl TestCoordinationDir {
        fn new(_name: &str) -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);

            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_micros();
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("lsi-{:x}-{timestamp:x}-{id:x}", std::process::id()));
            Self { path }
        }
    }

    #[cfg(unix)]
    impl Drop for TestCoordinationDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_guard_rejects_overlap_and_forwards_only_foreground_activation() {
        let temp = TestCoordinationDir::new("overlap");
        let primary = super::acquire_desktop_single_instance_at(&temp.path, false)
            .expect("acquire primary")
            .expect("primary guard");
        let receiver = primary.activation_receiver().expect("clone receiver");

        assert!(
            super::acquire_desktop_single_instance_at(&temp.path, false)
                .expect("foreground overlap")
                .is_none()
        );
        let (mut activation, _) = receiver
            .activation_listener
            .accept()
            .expect("accept foreground activation");
        let mut message = String::new();
        activation
            .read_to_string(&mut message)
            .expect("read activation");
        assert_eq!(message, "activate");

        assert!(
            super::acquire_desktop_single_instance_at(&temp.path, true)
                .expect("background overlap")
                .is_none()
        );
        receiver
            .activation_listener
            .set_nonblocking(true)
            .expect("make receiver nonblocking");
        let error = receiver
            .activation_listener
            .accept()
            .expect_err("background overlap must not activate");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    }

    #[cfg(unix)]
    #[test]
    fn unix_guard_rejects_overlap_before_receiver_endpoint_exists() {
        let temp = TestCoordinationDir::new("receiver-race");
        super::ensure_private_coordination_dir(&temp.path).expect("create coordination dir");
        let lock = super::open_coordination_lock(&temp.path.join("lock")).expect("open lock");
        super::try_lock_coordination_file(&lock).expect("claim lock");

        assert!(
            super::acquire_desktop_single_instance_at(&temp.path, false)
                .expect("overlap without receiver")
                .is_none(),
            "the lock loser must exit even when activation forwarding is not ready"
        );
    }

    #[cfg(unix)]
    #[test]
    fn relaunch_release_drops_ownership_before_a_successor_acquires_it() {
        let temp = TestCoordinationDir::new("relaunch-release");
        let primary = super::acquire_desktop_single_instance_at(&temp.path, true)
            .expect("acquire primary")
            .expect("primary guard");
        let state = super::DesktopSingleInstanceState::new(Some(primary));

        assert!(
            super::acquire_desktop_single_instance_at(&temp.path, true)
                .expect("overlap before release")
                .is_none()
        );
        assert!(state.release_for_relaunch().expect("release ownership"));

        let successor = super::acquire_desktop_single_instance_at(&temp.path, true)
            .expect("acquire successor")
            .expect("successor guard");
        assert!(!state.release_for_relaunch().expect("idempotent release"));
        drop(successor);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_coordination_falls_back_to_the_user_home() {
        let home = std::path::PathBuf::from("/home/ada");
        let path = super::linux_desktop_single_instance_coordination_dir(None, Some(home.clone()))
            .expect("home fallback");

        assert_eq!(path, home.join(".locality-desktop-si"));
        assert!(!path.starts_with("/tmp"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_coordination_requires_a_private_base() {
        let error = super::linux_desktop_single_instance_coordination_dir(
            Some(std::path::PathBuf::from("relative-runtime")),
            None,
        )
        .expect_err("relative runtime without a home must fail");

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_coordination_endpoint_uses_the_private_user_temp_directory() {
        use std::os::unix::ffi::OsStrExt;

        let user_temp_dir = super::macos_user_temp_dir().expect("private user temp directory");
        let path = super::desktop_single_instance_coordination_dir()
            .expect("macOS coordination directory");

        assert_eq!(path, user_temp_dir.join("locality-si"));
        assert!(
            path.join("activate.sock").as_os_str().as_bytes().len()
                <= super::DARWIN_UNIX_SOCKET_PATH_MAX
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_coordination_endpoint_rejects_an_unbounded_socket_path() {
        let oversized_base = std::path::Path::new("/").join("x".repeat(103));
        let error = super::macos_desktop_single_instance_coordination_dir(&oversized_base)
            .expect_err("oversized activation socket path must fail");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[cfg(windows)]
    #[test]
    fn windows_coordination_objects_are_session_scoped_and_null_terminated() {
        assert!(super::WINDOWS_DESKTOP_SINGLE_INSTANCE_MUTEX.starts_with(r"Local\"));
        assert!(super::WINDOWS_DESKTOP_ACTIVATION_EVENT.starts_with(r"Local\"));

        let wide = super::windows_wide_null("Locality");
        let expected = "Locality"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        assert_eq!(wide, expected);
    }
}
