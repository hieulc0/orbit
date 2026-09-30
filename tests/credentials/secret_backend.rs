use anyhow::Result;
use orbit::secret_backend::{LocalPrivateSecretBackend, SecretBackend, SecretBytes, SecretLocator};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
};

fn fixture() -> Result<(tempfile::TempDir, LocalPrivateSecretBackend, SecretLocator)> {
    let home = tempfile::tempdir()?;
    let backend = LocalPrivateSecretBackend::under_home(home.path())?;
    let locator = SecretLocator::new(
        &uuid::Uuid::new_v4().to_string(),
        1,
        &uuid::Uuid::new_v4().to_string(),
    )?;
    Ok((home, backend, locator))
}

#[tokio::test]
async fn local_private_permissions_atomicity_and_redaction() -> Result<()> {
    let (home, backend, locator) = fixture()?;
    let root = home.path().join(".orbit/private/credentials");
    assert_eq!(fs::metadata(&root)?.permissions().mode() & 0o777, 0o700);
    assert!(!backend.exists(locator).await?);
    let marker = "private-pat-do-not-print";
    let secret = SecretBytes::new(marker.as_bytes().to_vec())?;
    assert!(!format!("{secret:?}").contains(marker));
    backend.create(locator, secret).await?;
    assert_eq!(backend.read(locator).await?.expose(), marker.as_bytes());
    let error = backend
        .create(locator, SecretBytes::new(b"other".to_vec())?)
        .await
        .unwrap_err();
    assert!(!format!("{error:?}").contains(marker));
    assert_eq!(backend.read(locator).await?.expose(), marker.as_bytes());
    let credential_dir = root.join(locator.encode().split('/').nth(2).unwrap());
    let generation_dir = credential_dir.join("generation-1");
    let secret_file = generation_dir.join(locator.encode().split('/').nth(5).unwrap());
    assert_eq!(
        fs::metadata(&credential_dir)?.permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&generation_dir)?.permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&secret_file)?.permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::metadata(&secret_file)?.nlink(), 1);
    assert_eq!(fs::read_dir(&generation_dir)?.count(), 1);
    fs::set_permissions(&secret_file, fs::Permissions::from_mode(0o644))?;
    assert!(backend.read(locator).await.is_err());
    assert!(backend.exists(locator).await.is_err());
    fs::set_permissions(&secret_file, fs::Permissions::from_mode(0o600))?;
    backend
        .replace(locator, SecretBytes::new(b"replacement".to_vec())?)
        .await?;
    assert_eq!(backend.read(locator).await?.expose(), b"replacement");
    backend.delete(locator).await?;
    assert!(!backend.exists(locator).await?);
    Ok(())
}

#[tokio::test]
async fn local_private_does_not_touch_manual_credential_layout() -> Result<()> {
    let home = tempfile::tempdir()?;
    let manual = home.path().join(".orbit/credentials/manual-test");
    fs::create_dir_all(&manual)?;
    fs::set_permissions(
        home.path().join(".orbit"),
        fs::Permissions::from_mode(0o700),
    )?;
    let sentinel = manual.join("sentinel");
    fs::write(&sentinel, b"legacy-unmodified")?;
    let backend = LocalPrivateSecretBackend::under_home(home.path())?;
    let locator = SecretLocator::new(
        &uuid::Uuid::new_v4().to_string(),
        1,
        &uuid::Uuid::new_v4().to_string(),
    )?;
    backend
        .create(locator, SecretBytes::new(b"new-private".to_vec())?)
        .await?;
    assert_eq!(fs::read(sentinel)?, b"legacy-unmodified");
    Ok(())
}

#[tokio::test]
async fn local_private_rejects_unsafe_roots_and_symlink_escape() -> Result<()> {
    let (home, backend, locator) = fixture()?;
    let root = home.path().join(".orbit/private/credentials");
    let credential_id = locator.encode().split('/').nth(2).unwrap().to_string();
    let outside = tempfile::tempdir()?;
    symlink(outside.path(), root.join(&credential_id))?;
    assert!(
        backend
            .create(locator, SecretBytes::new(b"secret".to_vec())?)
            .await
            .is_err()
    );
    assert!(backend.exists(locator).await.is_err());
    assert_eq!(fs::read_dir(outside.path())?.count(), 0);
    fs::remove_file(root.join(credential_id))?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755))?;
    assert!(
        backend
            .create(locator, SecretBytes::new(b"secret".to_vec())?)
            .await
            .is_err()
    );
    assert!(LocalPrivateSecretBackend::under_home(home.path()).is_err());
    Ok(())
}

#[test]
fn logical_locators_reject_path_traversal_and_generation_aliases() -> Result<()> {
    let id = uuid::Uuid::new_v4().to_string();
    let secret = uuid::Uuid::new_v4().to_string();
    let locator = SecretLocator::new(&id, 2, &secret)?;
    assert!(locator.belongs_to(&id, 2));
    assert!(!locator.belongs_to(&id, 1));
    assert_eq!(SecretLocator::parse(&locator.encode())?, locator);
    for invalid in [
        format!("credential://../generation/2/{secret}"),
        format!("credential://{id}/generation/02/{secret}"),
        format!("credential://{id}/generation/2/../../{secret}"),
        format!("credential://{id}/generation/0/{secret}"),
        format!("/tmp/{id}"),
    ] {
        assert!(SecretLocator::parse(&invalid).is_err());
    }
    assert_eq!(format!("{locator:?}"), "SecretLocator([opaque])");
    Ok(())
}
