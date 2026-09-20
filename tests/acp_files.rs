use anyhow::Result;
use orbit::acp_files::{MAX_LINE_BYTES, MAX_LINE_LIMIT, MAX_RESPONSE_BYTES, Root};
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
    assert_eq!(broker.read("src", 65536)?, b"file");
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

#[test]
fn acp_files_read_text_range_on_large_file() -> Result<()> {
    let root = tempfile::tempdir()?;
    let broker = Root::open(root.path())?;

    // Create a 100 KB file with 1,000 lines (each line ~100 bytes)
    let mut large_content = String::new();
    for i in 1..=1000 {
        use std::fmt::Write;
        writeln!(
            &mut large_content,
            "line {:04}: {}",
            i,
            "abcdefghijklmnopqrstuvwxyz0123456789".repeat(2)
        )?;
    }
    assert!(
        large_content.len() > 65536,
        "test file must exceed 64 KB (is {} bytes)",
        large_content.len()
    );
    std::fs::write(root.path().join("large.txt"), &large_content)?;

    // 1. Read lines 1-120 (the exact operation that failed in dogfood #1)
    let range1 = broker.read_text_range("large.txt", 1, 120, MAX_RESPONSE_BYTES, MAX_LINE_BYTES)?;
    let lines1: Vec<&str> = range1.lines().collect();
    assert_eq!(lines1.len(), 120);
    assert!(lines1[0].starts_with("line 0001:"));
    assert!(lines1[119].starts_with("line 0120:"));

    // 2. Read lines 501-550 (later slice on large file)
    let range2 =
        broker.read_text_range("large.txt", 501, 50, MAX_RESPONSE_BYTES, MAX_LINE_BYTES)?;
    let lines2: Vec<&str> = range2.lines().collect();
    assert_eq!(lines2.len(), 50);
    assert!(lines2[0].starts_with("line 0501:"));
    assert!(lines2[49].starts_with("line 0550:"));

    // 3. Read past EOF returns empty string
    let past_eof =
        broker.read_text_range("large.txt", 2000, 50, MAX_RESPONSE_BYTES, MAX_LINE_BYTES)?;
    assert!(past_eof.is_empty());

    // 4. Invalid line (0) or limit (0 or > MAX_LINE_LIMIT)
    assert!(
        broker
            .read_text_range("large.txt", 0, 10, MAX_RESPONSE_BYTES, MAX_LINE_BYTES)
            .is_err()
    );
    assert!(
        broker
            .read_text_range("large.txt", 1, 0, MAX_RESPONSE_BYTES, MAX_LINE_BYTES)
            .is_err()
    );
    assert!(
        broker
            .read_text_range(
                "large.txt",
                1,
                MAX_LINE_LIMIT + 1,
                MAX_RESPONSE_BYTES,
                MAX_LINE_BYTES
            )
            .is_err()
    );

    // 5. Response byte limit enforcement
    assert!(
        broker
            .read_text_range("large.txt", 1, 1000, 1024, MAX_LINE_BYTES)
            .is_err()
    );

    // 6. Path traversal protection
    assert!(
        broker
            .read_text_range("../outside.txt", 1, 10, MAX_RESPONSE_BYTES, MAX_LINE_BYTES)
            .is_err()
    );

    // 7. Pathological line exceeding MAX_LINE_BYTES
    let long_line_content = format!("{}\nshort line\n", "x".repeat(MAX_LINE_BYTES + 10));
    std::fs::write(root.path().join("long_line.txt"), &long_line_content)?;
    assert!(
        broker
            .read_text_range("long_line.txt", 1, 10, MAX_RESPONSE_BYTES, MAX_LINE_BYTES)
            .is_err()
    );

    Ok(())
}
