//! Linux directory-fd-relative file access for the ACP workspace and auth broker.
//! No symlink or mount traversal, device/FIFO access, or multiply-linked files.
use anyhow::{Context, Result, ensure};
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
};

pub struct Root(File);
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

impl Root {
    pub fn open(path: &Path) -> Result<Self> {
        ensure!(
            path.is_absolute() && path.canonicalize()? == path,
            "file broker requires canonical root"
        );
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        ensure!(
            file.metadata()?.is_dir(),
            "file broker root is not a directory"
        );
        Ok(Self(file))
    }
    fn file(&self, path: &str, flags: i32, mode: u64) -> Result<File> {
        ensure!(
            crate::acp_runtime::relative_file(path),
            "file broker path must be relative normal components"
        );
        let path = CString::new(path)?;
        let how = OpenHow {
            flags: (flags | libc::O_CLOEXEC | libc::O_NONBLOCK) as u64,
            mode,
            resolve: 0x08 | 0x04 | 0x01,
        }; // BENEATH | NO_SYMLINKS | NO_XDEV
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                self.0.as_raw_fd(),
                path.as_ptr(),
                &how,
                std::mem::size_of::<OpenHow>(),
            )
        } as i32;
        ensure!(
            fd >= 0,
            "confined file open failed: {}",
            std::io::Error::last_os_error()
        );
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        ensure!(
            (metadata.is_file()
                && metadata.nlink() == 1
                && metadata.uid() == unsafe { libc::geteuid() })
                || (flags == libc::O_RDONLY
                    && metadata.is_dir()
                    && metadata.uid() == unsafe { libc::geteuid() }),
            "broker file must be an owned regular file with one link or directory"
        );
        Ok(file)
    }
    pub fn read(&self, path: &str, max: usize) -> Result<Vec<u8>> {
        let file = self.file(path, libc::O_RDONLY, 0)?;
        let metadata = file.metadata()?;
        if metadata.is_dir() {
            let proc_path = format!("/proc/self/fd/{}", file.as_raw_fd());
            let mut entries = Vec::new();
            for entry in std::fs::read_dir(proc_path)? {
                let entry = entry?;
                let file_name = entry.file_name();
                let name = file_name.to_string_lossy();
                if !name.starts_with('.') {
                    let is_dir = entry.file_type()?.is_dir();
                    if is_dir {
                        entries.push(format!("{name}/"));
                    } else {
                        entries.push(name.into_owned());
                    }
                }
            }
            entries.sort();
            let listing = entries.join(
                "
",
            );
            ensure!(listing.len() <= max, "broker file exceeds bound");
            return Ok(listing.into_bytes());
        }
        ensure!(metadata.len() <= max as u64, "broker file exceeds bound");
        let mut content = Vec::new();
        file.take(max as u64 + 1).read_to_end(&mut content)?;
        ensure!(content.len() <= max, "broker file exceeds bound");
        Ok(content)
    }
    pub fn read_private(&self, path: &str, max: usize) -> Result<Vec<u8>> {
        let file = self.file(path, libc::O_RDONLY, 0)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.mode() & 0o077 == 0,
            "auth file must be private"
        );
        let mut content = Vec::new();
        file.take(max as u64 + 1).read_to_end(&mut content)?;
        ensure!(content.len() <= max, "auth file exceeds bound");
        Ok(content)
    }
    pub fn write(&self, path: &str, content: &[u8]) -> Result<()> {
        ensure!(content.len() <= 65536, "broker write exceeds 64 KiB");
        // Open without truncation, validate the actual fd, only then mutate it.
        let mut file = self.file(path, libc::O_WRONLY | libc::O_CREAT, 0o600)?;
        file.set_len(0)?;
        file.write_all(content)?;
        file.sync_all()?;
        Ok(())
    }
    /// Replace only a previously provisioned private auth file, on the same
    /// directory fd. A crash cannot leave the original credential half-written.
    pub fn replace_private(&self, path: &str, content: &[u8]) -> Result<()> {
        self.read_private(path, 65536)?;
        ensure!(content.len() <= 65536, "auth refresh exceeds bound");
        let name = Path::new(path)
            .file_name()
            .context("auth filename missing")?
            .to_str()
            .context("auth filename is not UTF-8")?;
        let parent = Path::new(path).parent().unwrap();
        let dir = if parent.as_os_str().is_empty() {
            self.0.try_clone()?
        } else {
            let parent = CString::new(parent.to_str().context("auth path is not UTF-8")?)?;
            let how = OpenHow {
                flags: (libc::O_DIRECTORY | libc::O_RDONLY | libc::O_CLOEXEC) as u64,
                mode: 0,
                resolve: 0x08 | 0x04 | 0x01,
            };
            let fd = unsafe {
                libc::syscall(
                    libc::SYS_openat2,
                    self.0.as_raw_fd(),
                    parent.as_ptr(),
                    &how,
                    std::mem::size_of::<OpenHow>(),
                )
            } as i32;
            ensure!(fd >= 0, "auth parent confinement failed");
            unsafe { File::from_raw_fd(fd) }
        };
        let temporary = CString::new(format!(".orbit-refresh-{}", crate::model::id()))?;
        let name = CString::new(name)?;
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                temporary.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        ensure!(fd >= 0, "auth refresh staging failed");
        let result = (|| -> Result<()> {
            let mut file = unsafe { File::from_raw_fd(fd) };
            file.write_all(content)?;
            file.sync_all()?;
            ensure!(
                unsafe {
                    libc::renameat(
                        dir.as_raw_fd(),
                        temporary.as_ptr(),
                        dir.as_raw_fd(),
                        name.as_ptr(),
                    )
                } == 0,
                "atomic auth refresh failed"
            );
            dir.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            unsafe { libc::unlinkat(dir.as_raw_fd(), temporary.as_ptr(), 0) };
        }
        result
    }
}
