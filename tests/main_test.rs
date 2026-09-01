use wgtui::WingetPackage;

#[test]
fn test_winget_package_struct() {
    let pkg = WingetPackage {
        name: "Test".to_string(),
        id: "Test.ID".to_string(),
        version: Some("1.0".to_string()),
        source: Some("winget".to_string()),
    };
    assert_eq!(pkg.name, "Test");
    assert_eq!(pkg.id, "Test.ID");
    assert_eq!(pkg.version.as_deref(), Some("1.0"));
    assert_eq!(pkg.source.as_deref(), Some("winget"));
}

#[test]
fn test_check_winget_smoke() {
    // Just ensure it doesn't panic
    let _ = wgtui::check_winget();
}

#[test]
fn example_manifest_loads_and_is_well_formed() {
    let pkgs = wgtui::load_packages_from_file(std::path::Path::new("examples/packages.json"));
    assert!(pkgs.iter().any(|p| p.id == "Google.Chrome" && !p.is_script));
    assert!(pkgs.iter().any(|p| p.is_script));

    let java = pkgs
        .iter()
        .find(|p| p.id == "Oracle.JavaRuntimeEnvironment")
        .expect("java entry");
    assert!(java.install_args().contains(&"x86".to_string()));
}
