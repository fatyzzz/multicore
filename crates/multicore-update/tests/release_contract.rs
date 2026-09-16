use multicore_release::{GitHubRelease, ReleaseDecision, Repository, Version};

#[test]
fn repository_accepts_one_owner_and_repository() {
    let repository = Repository::parse("Fatyzzz/multi-core").unwrap();
    assert_eq!(repository.owner(), "Fatyzzz");
    assert_eq!(repository.name(), "multi-core");

    for invalid in ["", "owner", "owner/repo/extra", "../repo", "owner repo/x"] {
        assert!(Repository::parse(invalid).is_err(), "accepted {invalid:?}");
    }
}

#[test]
fn stable_versions_compare_numerically() {
    assert!(Version::parse("v1.10.0").unwrap() > Version::parse("0.9.9").unwrap());
    assert!(Version::parse("1.2.3").is_ok());
    for invalid in ["1.2", "v1.2.3-beta.1", "latest", "1.2.3.4"] {
        assert!(Version::parse(invalid).is_err(), "accepted {invalid:?}");
    }
}

#[test]
fn release_requires_exact_asset_size_and_sha256_digest() {
    let body = br#"{
        "tag_name":"v0.2.0",
        "html_url":"https://github.com/Fatyzzz/multi-core/releases/tag/v0.2.0",
        "assets":[{
            "name":"multicore-windows-x64.zip",
            "browser_download_url":"https://github.com/Fatyzzz/multi-core/releases/download/v0.2.0/multicore-windows-x64.zip",
            "size":1234,
            "digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }]
    }"#;

    let release = GitHubRelease::parse(body).unwrap();
    assert_eq!(release.version().to_string(), "0.2.0");
    assert_eq!(release.asset().size(), 1234);
    assert_eq!(release.asset().sha256_hex(), "a".repeat(64));
    assert!(matches!(
        release.decision(&Version::parse("0.1.0").unwrap()),
        ReleaseDecision::UpdateAvailable
    ));
    assert!(matches!(
        release.decision(&Version::parse("0.2.0").unwrap()),
        ReleaseDecision::Current
    ));
}

#[test]
fn release_rejects_missing_or_non_sha256_digest() {
    for digest in ["", "sha512:abcd", "sha256:abcd"] {
        let body = format!(
            r#"{{"tag_name":"v0.2.0","html_url":"https://github.com/o/r/releases/tag/v0.2.0","assets":[{{"name":"multicore-windows-x64.zip","browser_download_url":"https://github.com/o/r/releases/download/v0.2.0/multicore-windows-x64.zip","size":12,"digest":"{digest}"}}]}}"#
        );
        assert!(GitHubRelease::parse(body.as_bytes()).is_err());
    }
}

#[test]
fn pinned_repository_rejects_cross_repository_release_urls() {
    let repository = Repository::parse("owner/repo").unwrap();
    let body = br#"{
        "tag_name":"v0.2.0",
        "html_url":"https://github.com/attacker/repo/releases/tag/v0.2.0",
        "assets":[{
            "name":"multicore-windows-x64.zip",
            "browser_download_url":"https://github.com/attacker/repo/releases/download/v0.2.0/multicore-windows-x64.zip",
            "size":12,
            "digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }]
    }"#;
    assert!(GitHubRelease::parse_for_repository(&repository, body).is_err());
}
