const APP_MANIFEST: &str = include_str!("../app.manifest");
const DEBUG_MANIFEST: &str = include_str!("../app-debug.manifest");
const BUILD_SCRIPT: &str = include_str!("../build.rs");

#[test]
fn desktop_requests_elevation_from_windows_automatically() {
    assert!(
        APP_MANIFEST.contains(
            r#"<requestedExecutionLevel level="requireAdministrator" uiAccess="false" />"#
        ),
        "MultiCore must trigger the standard Windows UAC prompt when launched normally"
    );
    assert!(
        !APP_MANIFEST.contains(r#"level="asInvoker""#),
        "asInvoker leaves Mihomo TUN without the administrator token it requires"
    );
}

#[test]
fn desktop_build_embeds_the_elevation_manifest() {
    assert!(
        BUILD_SCRIPT.contains(r#""1 24 \"{}\""#),
        "the manifest must use numeric PE resource type 24; an unexpanded RT_MANIFEST token becomes an inert string resource"
    );
    assert!(BUILD_SCRIPT.contains("app.manifest"));
    assert!(BUILD_SCRIPT.contains("manifest_required()"));
}

#[test]
fn debug_and_test_builds_do_not_require_elevation() {
    assert!(BUILD_SCRIPT.contains(r#"std::env::var("PROFILE")"#));
    assert!(BUILD_SCRIPT.contains("app-debug.manifest"));
    assert!(
        DEBUG_MANIFEST
            .contains(r#"<requestedExecutionLevel level="asInvoker" uiAccess="false" />"#)
    );
    assert!(!DEBUG_MANIFEST.contains(r#"level="requireAdministrator""#));
}
