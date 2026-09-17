const DESKTOP_MANIFEST: &str = include_str!("../app.manifest");
const DESKTOP_DEBUG_MANIFEST: &str = include_str!("../app-debug.manifest");
const DESKTOP_BUILD_SCRIPT: &str = include_str!("../build.rs");
const CORE_HOST_MANIFEST: &str = include_str!("../../multicore-core-host/app.manifest");
const CORE_HOST_BUILD_SCRIPT: &str = include_str!("../../multicore-core-host/build.rs");
const CORE_HOST_MAIN: &str = include_str!("../../multicore-core-host/src/main.rs");
const UPDATER_MANIFEST: &str = include_str!("../../multicore-updater/app.manifest");
const DAEMON_MANIFEST: &str = include_str!("../../../crates/multicore-daemon/app.manifest");
const DAEMON_BUILD_SCRIPT: &str = include_str!("../../../crates/multicore-daemon/build.rs");
const PE_MANIFEST_TEST: &str = include_str!("../../../scripts/test-windows-elevation-manifest.ps1");

const AS_INVOKER: &str = r#"<requestedExecutionLevel level="asInvoker" uiAccess="false" />"#;
const REQUIRE_ADMINISTRATOR: &str =
    r#"<requestedExecutionLevel level="requireAdministrator" uiAccess="false" />"#;

#[test]
fn desktop_release_and_debug_are_as_invoker() {
    for (name, manifest) in [
        ("release", DESKTOP_MANIFEST),
        ("debug", DESKTOP_DEBUG_MANIFEST),
    ] {
        assert!(
            manifest.contains(AS_INVOKER),
            "desktop {name} must be asInvoker"
        );
        assert!(
            !manifest.contains(REQUIRE_ADMINISTRATOR),
            "desktop {name} must not request elevation"
        );
    }
}

#[test]
fn desktop_build_embeds_numeric_manifest_resource_one() {
    assert!(DESKTOP_BUILD_SCRIPT.contains(r#""1 24 \"{}\""#));
    assert!(DESKTOP_BUILD_SCRIPT.contains("app.manifest"));
    assert!(DESKTOP_BUILD_SCRIPT.contains("app-debug.manifest"));
    assert!(DESKTOP_BUILD_SCRIPT.contains("manifest_required()"));
}

#[test]
fn core_host_always_requires_administrator_and_embeds_numeric_manifest_resource_one() {
    assert!(CORE_HOST_MANIFEST.contains(REQUIRE_ADMINISTRATOR));
    assert!(!CORE_HOST_MANIFEST.contains(AS_INVOKER));
    assert!(CORE_HOST_BUILD_SCRIPT.contains(r#""1 24 \"{}\""#));
    assert!(CORE_HOST_BUILD_SCRIPT.contains("app.manifest"));
    assert!(CORE_HOST_BUILD_SCRIPT.contains("manifest_required()"));
    assert!(!CORE_HOST_BUILD_SCRIPT.contains(r#"std::env::var("PROFILE")"#));
}

#[test]
fn core_host_stub_fails_closed_without_accepting_or_echoing_arguments() {
    assert!(CORE_HOST_MAIN.contains("std::process::exit(1)"));
    assert!(!CORE_HOST_MAIN.contains("std::env::args"));
    assert!(!CORE_HOST_MAIN.contains("std::env::vars"));
}

#[test]
fn updater_and_daemon_do_not_request_elevation() {
    assert!(UPDATER_MANIFEST.contains(AS_INVOKER));
    assert!(!UPDATER_MANIFEST.contains(REQUIRE_ADMINISTRATOR));
    assert!(DAEMON_MANIFEST.contains(AS_INVOKER));
    assert!(!DAEMON_MANIFEST.contains(REQUIRE_ADMINISTRATOR));
    assert!(DAEMON_BUILD_SCRIPT.contains(r#""1 24 \"{}\""#));
    assert!(DAEMON_BUILD_SCRIPT.contains("app.manifest"));
    assert!(DAEMON_BUILD_SCRIPT.contains("manifest_required()"));
}

#[test]
fn pe_contract_checks_all_three_executables_by_numeric_resource_identifiers() {
    assert!(PE_MANIFEST_TEST.contains("$DesktopExecutablePath"));
    assert!(PE_MANIFEST_TEST.contains("$CoreHostExecutablePath"));
    assert!(PE_MANIFEST_TEST.contains("$DaemonExecutablePath"));
    assert!(PE_MANIFEST_TEST.contains("Assert-Amd64PeExecutable"));
    assert!(PE_MANIFEST_TEST.contains("[IntPtr]1"));
    assert!(PE_MANIFEST_TEST.contains("[IntPtr]24"));
    assert!(PE_MANIFEST_TEST.contains("$asInvoker"));
    assert!(PE_MANIFEST_TEST.contains("$requireAdministrator"));
}
