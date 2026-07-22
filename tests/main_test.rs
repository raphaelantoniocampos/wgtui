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