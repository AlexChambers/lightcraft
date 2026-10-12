use std::collections::BTreeSet;

use plist::Value;

#[test]
fn macos_document_types_match_import_extensions_and_never_claim_defaults() {
    let plist = Value::from_reader_xml(include_bytes!("../../../packaging/macos/Info.plist.in").as_slice()).unwrap();
    let plist = plist.as_dictionary().unwrap();
    let mut registered = BTreeSet::new();
    let mut identifiers = BTreeSet::new();
    for entry in plist["CFBundleDocumentTypes"].as_array().unwrap() {
        let entry = entry.as_dictionary().unwrap();
        assert_eq!(entry["LSHandlerRank"].as_string(), Some("Alternate"));
        assert!(matches!(entry["CFBundleTypeRole"].as_string(), Some("Editor" | "Viewer")));
        registered.extend(strings(&entry["CFBundleTypeExtensions"]));
        identifiers.extend(strings(&entry["LSItemContentTypes"]));
    }
    let expected: BTreeSet<_> = lightcraft_engine::import::EXTENSIONS.iter().copied().collect();
    assert_eq!(registered, expected, "run python3 packaging/macos/document_types.py packaging/macos/Info.plist.in");
    let mut declared = BTreeSet::new();
    let mut tagged = BTreeSet::new();
    for entry in plist["UTImportedTypeDeclarations"].as_array().unwrap() {
        let entry = entry.as_dictionary().unwrap();
        let uti = entry["UTTypeIdentifier"].as_string().unwrap();
        assert!(declared.insert(uti), "duplicate UTI: {uti}");
        assert!(identifiers.contains(uti), "unused UTI: {uti}");
        let tags = entry["UTTypeTagSpecification"].as_dictionary().unwrap();
        for ext in strings(&tags["public.filename-extension"]) {
            assert!(tagged.insert(ext), "duplicate extension: {ext}");
        }
        let conforms = strings(&entry["UTTypeConformsTo"]);
        assert!(conforms.contains(&"public.data"));
        assert!(conforms.contains(&"public.image") || conforms.contains(&"public.camera-raw-image"));
    }
    assert_eq!(identifiers, declared);
    assert_eq!(tagged, expected);
}

fn strings(value: &Value) -> Vec<&str> {
    value.as_array().unwrap().iter().map(|v| v.as_string().unwrap()).collect()
}
