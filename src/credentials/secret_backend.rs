//! Opaque secret locators and an owner-only local backend. No provider logic or
//! physical path crosses the catalog boundary.
use anyhow::{Result, ensure};
use async_trait::async_trait;
use std::{
    ffi::{CStr, CString},
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
};
use uuid::Uuid;
use zeroize::Zeroize;

pub const LOCAL_PRIVATE_ID: &str = "local-private";
pub const MAX_SECRET_BYTES: usize = 1024 * 1024;
const RESOLVE_BENEATH_NO_LINKS_OR_MOUNTS: u64 = 0x08 | 0x04 | 0x01;

/// A logical address. Its UUID components cannot name a host path or escape
/// the backend root. Display and Debug deliberately reveal no locator string.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SecretLocator {
    credential_id: Uuid,
    generation: u64,
    secret_id: Uuid,
}

impl std::fmt::Debug for SecretLocator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretLocator([opaque])")
    }
}

impl SecretLocator {
    pub fn new(credential_id: &str, generation: u64, secret_id: &str) -> Result<Self> {
        ensure!(
            generation > 0 && generation <= i64::MAX as u64,
            "invalid secret locator"
        );
        Ok(Self {
            credential_id: Uuid::parse_str(credential_id)
                .map_err(|_| anyhow::anyhow!("invalid secret locator"))?,
            generation,
            secret_id: Uuid::parse_str(secret_id)
                .map_err(|_| anyhow::anyhow!("invalid secret locator"))?,
        })
    }

    pub fn parse(value: &str) -> Result<Self> {
        let parts: Vec<_> = value.split('/').collect();
        ensure!(
            parts.len() == 6
                && parts[0] == "credential:"
                && parts[1].is_empty()
                && parts[3] == "generation",
            "invalid secret locator"
        );
        let generation: u64 = parts[4]
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid secret locator"))?;
        let locator = Self::new(parts[2], generation, parts[5])?;
        ensure!(locator.encode() == value, "invalid secret locator");
        Ok(locator)
    }

    pub fn encode(self) -> String {
        format!(
            "credential://{}/generation/{}/{}",
            self.credential_id, self.generation, self.secret_id
        )
    }

    pub fn belongs_to(self, credential_id: &str, generation: u64) -> bool {
        self.generation == generation
            && Uuid::parse_str(credential_id).is_ok_and(|id| id == self.credential_id)
    }
}

/// Secret bytes cannot be serialized or printed. They are cleared on drop.
pub struct SecretBytes(Vec<u8>);

impl std::fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretBytes([REDACTED])")
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl SecretBytes {
    pub fn new(value: Vec<u8>) -> Result<Self> {
        let secret = Self(value);
        ensure!(
            !secret.0.is_empty() && secret.0.len() <= MAX_SECRET_BYTES,
            "invalid secret size"
        );
        Ok(secret)
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

#[async_trait]
pub trait SecretBackend: Send + Sync {
    fn backend_id(&self) -> &'static str;
    async fn create(&self, locator: SecretLocator, secret: SecretBytes) -> Result<()>;
    async fn read(&self, locator: SecretLocator) -> Result<SecretBytes>;
    async fn replace(&self, locator: SecretLocator, secret: SecretBytes) -> Result<()>;
    async fn delete(&self, locator: SecretLocator) -> Result<()>;
    async fn exists(&self, locator: SecretLocator) -> Result<bool>;
}

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

fn confined_open(parent: &File, name: &str, flags: i32) -> Result<Option<File>> {
    let name = CString::new(name).map_err(|_| anyhow::anyhow!("invalid private entry"))?;
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC | libc::O_NONBLOCK) as u64,
        mode: 0,
        resolve: RESOLVE_BENEATH_NO_LINKS_OR_MOUNTS,
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            parent.as_raw_fd(),
            name.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    } as i32;
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        anyhow::bail!("private entry unavailable or unsafe");
    }
    Ok(Some(unsafe { File::from_raw_fd(fd) }))
}

fn private_dir(parent: &File, name: &str, create: bool) -> Result<Option<File>> {
    let name_c = CString::new(name).map_err(|_| anyhow::anyhow!("invalid private directory"))?;
    if create {
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), 0o700) } != 0 {
            let error = std::io::Error::last_os_error();
            ensure!(
                error.kind() == std::io::ErrorKind::AlreadyExists,
                "private directory creation failed"
            );
        } else {
            parent.sync_all()?;
        }
    }
    let Some(file) = confined_open(parent, name, libc::O_RDONLY | libc::O_DIRECTORY)? else {
        return Ok(None);
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o7777 == 0o700,
        "private directory owner or mode invalid"
    );
    Ok(Some(file))
}

fn checked_secret_file(parent: &File, name: &str) -> Result<Option<File>> {
    let Some(file) = confined_open(parent, name, libc::O_RDONLY)? else {
        return Ok(None);
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.nlink() == 1
            && metadata.mode() & 0o7777 == 0o600
            && metadata.len() > 0
            && metadata.len() <= MAX_SECRET_BYTES as u64,
        "private secret owner, mode or size invalid"
    );
    Ok(Some(file))
}

fn atomic_write(parent: &File, name: &str, secret: &SecretBytes, replace: bool) -> Result<()> {
    let destination = CString::new(name).map_err(|_| anyhow::anyhow!("invalid secret entry"))?;
    let temporary = CString::new(format!(".staging-{}", Uuid::new_v4()))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    ensure!(fd >= 0, "private secret staging failed");
    let result = (|| -> Result<()> {
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(secret.expose())?;
        file.sync_all()?;
        let status = if replace {
            unsafe {
                libc::renameat(
                    parent.as_raw_fd(),
                    temporary.as_ptr(),
                    parent.as_raw_fd(),
                    destination.as_ptr(),
                )
            }
        } else {
            unsafe {
                libc::renameat2(
                    parent.as_raw_fd(),
                    temporary.as_ptr(),
                    parent.as_raw_fd(),
                    destination.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            }
        };
        ensure!(status == 0, "atomic private secret publication failed");
        parent.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        unsafe { libc::unlinkat(parent.as_raw_fd(), temporary.as_ptr(), 0) };
    }
    result
}

/// The only physical layout owner. The root is held open by fd, and every
/// child is opened with the same no-link/no-mount confinement as the ACP broker.
#[derive(Clone)]
pub struct LocalPrivateSecretBackend {
    root: Arc<File>,
}

impl LocalPrivateSecretBackend {
    pub fn default_for_operator() -> Result<Self> {
        Self::under_home(&operator_home()?)
    }

    pub fn under_home(home: &Path) -> Result<Self> {
        ensure!(
            home.is_absolute() && home.canonicalize()? == home,
            "private home must be canonical"
        );
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(home)?;
        let metadata = directory.metadata()?;
        ensure!(
            metadata.is_dir()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o022 == 0,
            "private home owner or mode invalid"
        );
        let orbit = private_dir(&directory, ".orbit", true)?
            .ok_or_else(|| anyhow::anyhow!("private Orbit directory unavailable"))?;
        let private = private_dir(&orbit, "private", true)?
            .ok_or_else(|| anyhow::anyhow!("private Orbit root unavailable"))?;
        let credentials = private_dir(&private, "credentials", true)?
            .ok_or_else(|| anyhow::anyhow!("private credential root unavailable"))?;
        Ok(Self {
            root: Arc::new(credentials),
        })
    }

    fn directory(&self, locator: SecretLocator, create: bool) -> Result<Option<File>> {
        let metadata = self.root.metadata()?;
        ensure!(
            metadata.is_dir()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o7777 == 0o700,
            "private credential root owner or mode invalid"
        );
        let Some(credential) = private_dir(&self.root, &locator.credential_id.to_string(), create)?
        else {
            return Ok(None);
        };
        private_dir(
            &credential,
            &format!("generation-{}", locator.generation),
            create,
        )
    }

    fn create_sync(&self, locator: SecretLocator, secret: SecretBytes) -> Result<()> {
        let directory = self
            .directory(locator, true)?
            .ok_or_else(|| anyhow::anyhow!("private secret directory unavailable"))?;
        atomic_write(&directory, &locator.secret_id.to_string(), &secret, false)
    }

    fn read_sync(&self, locator: SecretLocator) -> Result<SecretBytes> {
        let directory = self
            .directory(locator, false)?
            .ok_or_else(|| anyhow::anyhow!("private secret unavailable"))?;
        let mut file = checked_secret_file(&directory, &locator.secret_id.to_string())?
            .ok_or_else(|| anyhow::anyhow!("private secret unavailable"))?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_SECRET_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        SecretBytes::new(bytes)
    }

    fn replace_sync(&self, locator: SecretLocator, secret: SecretBytes) -> Result<()> {
        let directory = self
            .directory(locator, false)?
            .ok_or_else(|| anyhow::anyhow!("private secret unavailable"))?;
        checked_secret_file(&directory, &locator.secret_id.to_string())?
            .ok_or_else(|| anyhow::anyhow!("private secret unavailable"))?;
        atomic_write(&directory, &locator.secret_id.to_string(), &secret, true)
    }

    fn delete_sync(&self, locator: SecretLocator) -> Result<()> {
        let Some(directory) = self.directory(locator, false)? else {
            return Ok(());
        };
        if checked_secret_file(&directory, &locator.secret_id.to_string())?.is_none() {
            return Ok(());
        }
        let name = CString::new(locator.secret_id.to_string())?;
        ensure!(
            unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } == 0,
            "private secret deletion failed"
        );
        directory.sync_all()?;
        Ok(())
    }

    fn exists_sync(&self, locator: SecretLocator) -> Result<bool> {
        let Some(directory) = self.directory(locator, false)? else {
            return Ok(false);
        };
        Ok(checked_secret_file(&directory, &locator.secret_id.to_string())?.is_some())
    }
}

/// Resolve Orbit's operator home independently of a caller-provided `HOME`.
/// Services and shells for the same uid therefore address the same private
/// credential store. `ORBIT_HOME` is the explicit installation override for
/// deployments whose private Orbit state intentionally lives elsewhere.
pub fn operator_home() -> Result<PathBuf> {
    let home = if let Some(configured) = std::env::var_os("ORBIT_HOME") {
        PathBuf::from(configured)
    } else {
        account_home()?
    };
    ensure!(
        home.is_absolute() && home.canonicalize()? == home,
        "Orbit home must be absolute and canonical"
    );
    let metadata = std::fs::symlink_metadata(&home)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o022 == 0,
        "Orbit home owner or mode invalid"
    );
    Ok(home)
}

fn account_home() -> Result<PathBuf> {
    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let buffer_len = if suggested > 0 {
        usize::try_from(suggested)
            .unwrap_or(16 * 1024)
            .clamp(1024, 1024 * 1024)
    } else {
        16 * 1024
    };
    let mut buffer = vec![0_u8; buffer_len];
    let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            record.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    ensure!(
        status == 0 && !result.is_null(),
        "Orbit account home unavailable"
    );
    let record = unsafe { record.assume_init() };
    ensure!(!record.pw_dir.is_null(), "Orbit account home unavailable");
    let home = unsafe { CStr::from_ptr(record.pw_dir) }
        .to_str()
        .map_err(|_| anyhow::anyhow!("Orbit account home is not UTF-8"))?;
    ensure!(!home.is_empty(), "Orbit account home unavailable");
    Ok(PathBuf::from(home))
}

#[async_trait]
impl SecretBackend for LocalPrivateSecretBackend {
    fn backend_id(&self) -> &'static str {
        LOCAL_PRIVATE_ID
    }

    async fn create(&self, locator: SecretLocator, secret: SecretBytes) -> Result<()> {
        let backend = self.clone();
        tokio::task::spawn_blocking(move || backend.create_sync(locator, secret))
            .await
            .map_err(|_| anyhow::anyhow!("private secret worker failed"))?
    }

    async fn read(&self, locator: SecretLocator) -> Result<SecretBytes> {
        let backend = self.clone();
        tokio::task::spawn_blocking(move || backend.read_sync(locator))
            .await
            .map_err(|_| anyhow::anyhow!("private secret worker failed"))?
    }

    async fn replace(&self, locator: SecretLocator, secret: SecretBytes) -> Result<()> {
        let backend = self.clone();
        tokio::task::spawn_blocking(move || backend.replace_sync(locator, secret))
            .await
            .map_err(|_| anyhow::anyhow!("private secret worker failed"))?
    }

    async fn delete(&self, locator: SecretLocator) -> Result<()> {
        let backend = self.clone();
        tokio::task::spawn_blocking(move || backend.delete_sync(locator))
            .await
            .map_err(|_| anyhow::anyhow!("private secret worker failed"))?
    }

    async fn exists(&self, locator: SecretLocator) -> Result<bool> {
        let backend = self.clone();
        tokio::task::spawn_blocking(move || backend.exists_sync(locator))
            .await
            .map_err(|_| anyhow::anyhow!("private secret worker failed"))?
    }
}
