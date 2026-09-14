use anyhow::Result;
use orbit::acp_files::Root;
use std::os::unix::fs::{PermissionsExt, symlink};

#[test]
fn acp_files_reject_escape_symlink_fifo_hardlink_and_support_private_refresh() -> Result<()> {
    let root = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    std::fs::write(outside.path().join("secret"), "outside")?;
    std::fs::create_dir(root.path().join("src"))?;
    let broker = Root::open(root.path())?;
    broker.write("src/file", b"first")?;
    assert_eq!(broker.read("src/file", 65536)?, b"first");
    broker.write("src/file", b"next")?;
    assert_eq!(broker.read("src/file", 65536)?, b"next");
    symlink(outside.path(), root.path().join("link"))?;
    std::fs::hard_link(outside.path().join("secret"), root.path().join("hard"))?;
    let fifo = std::ffi::CString::new(root.path().join("fifo").to_str().unwrap())?;
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    for path in ["../secret", "/etc/passwd", "link/secret", "hard", "fifo"] {
        assert!(broker.read(path, 65536).is_err(), "{path}");
        assert!(broker.write(path, b"changed").is_err(), "{path}");
    }
    assert_eq!(std::fs::read(outside.path().join("secret"))?, b"outside");
    assert!(broker.write("large", &vec![b'x'; 65537]).is_err());
    broker.replace_private("src/file", b"refreshed")?;
    assert_eq!(broker.read_private("src/file", 65536)?, b"refreshed");
    std::fs::set_permissions(
        root.path().join("src/file"),
        std::fs::Permissions::from_mode(0o644),
    )?;
    assert!(broker.read_private("src/file", 65536).is_err());
    assert!(broker.replace_private("src/file", b"bad").is_err());
    Ok(())
}

#[test]
fn acp_files_symlink_swap_never_reaches_an_outside_marker() -> Result<()> {
    let root = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    std::fs::write(outside.path().join("marker"), "private")?;
    std::fs::create_dir(root.path().join("safe"))?;
    std::fs::write(root.path().join("safe/marker"), "safe")?;
    let broker = Root::open(root.path())?;
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for _ in 0..200 {
                let _ = std::fs::rename(root.path().join("safe"), root.path().join("parked"));
                let _ = symlink(outside.path(), root.path().join("safe"));
                let _ = std::fs::remove_file(root.path().join("safe"));
                let _ = std::fs::rename(root.path().join("parked"), root.path().join("safe"));
            }
        });
        for _ in 0..200 {
            if let Ok(bytes) = broker.read("safe/marker", 65536) {
                assert_ne!(bytes, b"private");
            }
            let _ = broker.write("safe/marker", b"broker");
        }
    });
    assert_eq!(std::fs::read(outside.path().join("marker"))?, b"private");
    Ok(())
}
