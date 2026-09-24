use orbit::provider_scope::fingerprint;

#[test]
fn provider_scope_fingerprint_is_domain_separated_bounded_and_private() {
    let value = fingerprint("openai", "personal-account-opaque").unwrap();
    assert_eq!(
        value,
        fingerprint("openai", "personal-account-opaque").unwrap()
    );
    assert_ne!(
        value,
        fingerprint("other-provider", "personal-account-opaque").unwrap()
    );
    assert_ne!(value, fingerprint("openai", "other-account").unwrap());
    assert!(value.starts_with("ps1:"));
    assert_eq!(value.len(), 68);
    assert!(!value.contains("personal-account-opaque"));
    let oversized = "x".repeat(257);
    for invalid in ["", " ", " account", "account\n", oversized.as_str()] {
        assert!(fingerprint("openai", invalid).is_none());
    }
}
