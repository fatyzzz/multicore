const DESKTOP_BUILD_SCRIPT: &str = include_str!("../build.rs");
const CORE_HOST_BUILD_SCRIPT: &str = include_str!("../../multicore-core-host/build.rs");
const CORE_HOST_MAIN: &str = include_str!("../../multicore-core-host/src/main.rs");
const UPDATER_BUILD_SCRIPT: &str = include_str!("../../multicore-updater/build.rs");
const UPDATER_RESOURCE_SCRIPT: &str = include_str!("../../multicore-updater/windows-resource.rc");
const DAEMON_BUILD_SCRIPT: &str = include_str!("../../../crates/multicore-daemon/build.rs");
const PE_MANIFEST_TEST: &str = include_str!("../../../scripts/test-windows-elevation-manifest.ps1");

#[test]
fn desktop_build_embeds_numeric_manifest_resource_one() {
    assert!(DESKTOP_BUILD_SCRIPT.contains(r#""1 24 \"{}\""#));
    assert!(DESKTOP_BUILD_SCRIPT.contains("app.manifest"));
    assert!(DESKTOP_BUILD_SCRIPT.contains("app-debug.manifest"));
    assert!(DESKTOP_BUILD_SCRIPT.contains("manifest_required()"));
}

#[test]
fn core_host_embeds_numeric_manifest_resource_one_for_all_profiles() {
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
fn updater_and_daemon_embed_required_manifest_resources() {
    assert!(UPDATER_RESOURCE_SCRIPT.contains("1 24"));
    assert!(UPDATER_BUILD_SCRIPT.contains("windows-resource.rc"));
    assert!(UPDATER_BUILD_SCRIPT.contains("app.manifest"));
    assert!(UPDATER_BUILD_SCRIPT.contains("manifest_required()"));
    assert!(DAEMON_BUILD_SCRIPT.contains(r#""1 24 \"{}\""#));
    assert!(DAEMON_BUILD_SCRIPT.contains("app.manifest"));
    assert!(DAEMON_BUILD_SCRIPT.contains("manifest_required()"));
}

#[test]
fn pe_contract_checks_all_four_executables_by_numeric_resource_identifiers() {
    assert!(PE_MANIFEST_TEST.contains("$DesktopExecutablePath"));
    assert!(PE_MANIFEST_TEST.contains("$CoreHostExecutablePath"));
    assert!(PE_MANIFEST_TEST.contains("$DaemonExecutablePath"));
    assert!(PE_MANIFEST_TEST.contains("$UpdaterExecutablePath"));
    assert!(PE_MANIFEST_TEST.contains("Assert-Amd64PeExecutable"));
    assert!(PE_MANIFEST_TEST.contains("DtdProcessing"));
    assert!(PE_MANIFEST_TEST.contains("XmlResolver"));
    assert!(PE_MANIFEST_TEST.contains("UTF8Encoding"));
    assert!(PE_MANIFEST_TEST.contains("[IntPtr]1"));
    assert!(PE_MANIFEST_TEST.contains("[IntPtr]24"));
    assert!(PE_MANIFEST_TEST.contains("$asInvoker"));
    assert!(PE_MANIFEST_TEST.contains("$requireAdministrator"));
}
