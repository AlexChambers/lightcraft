use std::collections::BTreeSet;

use plist::Value;

#[test]
fn macos_document_types_match_import_extensions_and_use_alternate_rank() {
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
    // These types already have declarations in macOS 11's CoreTypes/RawCameraTypes bundles.
    // The document list must reference them without replacing the system's extension tags.
    let system: BTreeSet<_> = [
        "public.jpeg",
        "public.png",
        "public.tiff",
        "org.webmproject.webp",
        "com.adobe.raw-image",
        "com.canon.cr2-raw-image",
        "com.canon.cr3-raw-image",
        "com.nikon.raw-image",
        "com.nikon.nrw-raw-image",
        "com.sony.arw-raw-image",
        "com.fuji.raw-image",
        "com.olympus.raw-image",
        "com.panasonic.rw2-raw-image",
        "com.leica.rwl-raw-image",
        "com.panasonic.raw-image",
        "com.pentax.raw-image",
        "com.samsung.raw-image",
        "com.adobe.photoshop-image",
        "com.compuserve.gif",
        "com.microsoft.bmp",
        "public.heic",
        "public.heif",
        "com.phaseone.raw-image",
        "com.canon.crw-raw-image",
        "com.konicaminolta.raw-image",
        "com.leafamerica.raw-image",
        "com.epson.raw-image",
        "com.hasselblad.3fr-raw-image",
        "com.hasselblad.fff-raw-image",
    ]
    .into_iter()
    .collect();
    let fallbacks = [
        ("public.jpeg-xl", "jxl"),
        ("public.avif", "avif"),
        ("ai.storyteller.lightcraft.image.x3f", "x3f"),
        ("ai.storyteller.lightcraft.image.kdc", "kdc"),
    ];
    let mut declared = BTreeSet::new();
    let mut tagged = BTreeSet::new();
    for entry in plist["UTImportedTypeDeclarations"].as_array().unwrap() {
        let entry = entry.as_dictionary().unwrap();
        let uti = entry["UTTypeIdentifier"].as_string().unwrap();
        assert!(!system.contains(uti), "system type must not be redeclared: {uti}");
        assert_ne!(uti, "public.camera-raw-image", "never redeclare the system's raw base type");
        assert!(declared.insert(uti), "duplicate UTI: {uti}");
        assert!(identifiers.contains(uti), "unused UTI: {uti}");
        let tags = entry["UTTypeTagSpecification"].as_dictionary().unwrap();
        let (_, expected_tag) = fallbacks.iter().find(|(id, _)| *id == uti).unwrap();
        assert_eq!(strings(&tags["public.filename-extension"]), [*expected_tag], "wrong filename tag for {uti}");
        for ext in strings(&tags["public.filename-extension"]) {
            assert!(tagged.insert(ext), "duplicate extension: {ext}");
        }
        let conforms = strings(&entry["UTTypeConformsTo"]);
        assert!(conforms.contains(&"public.data"));
        assert!(conforms.contains(&"public.image") || conforms.contains(&"public.camera-raw-image"));
    }
    assert_eq!(declared, fallbacks.iter().map(|(uti, _)| *uti).collect());
    assert_eq!(tagged, fallbacks.iter().map(|(_, ext)| *ext).collect());
    assert_eq!(identifiers, system.union(&declared).copied().collect());
    assert!(tagged.is_subset(&expected));
}

fn strings(value: &Value) -> Vec<&str> {
    value.as_array().unwrap().iter().map(|v| v.as_string().unwrap()).collect()
}
