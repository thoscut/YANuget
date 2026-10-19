//! An override's script replaces the package's own, under the name the
//! package gives it.

use std::io::{Read, Write};
use std::path::Path;

use yanuget_internalize::overrides::with_script;

fn nupkg(path: &Path, script: Option<&str>) {
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("[Content_Types].xml", opts).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="utf-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="nuspec" ContentType="application/octet" /><Default Extension="ps1" ContentType="application/octet" /></Types>"#,
    )
    .unwrap();
    zip.start_file("tool.nuspec", opts).unwrap();
    zip.write_all(b"<package><metadata><id>tool</id><version>1.0.0</version></metadata></package>")
        .unwrap();
    if let Some(name) = script {
        zip.start_file(name, opts).unwrap();
        zip.write_all(b"Install-ChocolateyPackage upstream")
            .unwrap();
    }
    zip.finish().unwrap();
}

fn entries(path: &Path) -> Vec<(String, String)> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut out = Vec::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).unwrap();
        let mut text = String::new();
        f.read_to_string(&mut text).unwrap();
        out.push((f.name().unwrap().to_string(), text));
    }
    out
}

#[tokio::test]
async fn the_script_is_replaced_in_place_or_added() {
    let dir = tempfile::tempdir().unwrap();
    let (input, output) = (dir.path().join("in.nupkg"), dir.path().join("out.nupkg"));

    nupkg(&input, Some("tools/ChocolateyInstall.ps1"));
    with_script(&input, &output, b"Write-Host internal".to_vec())
        .await
        .unwrap();
    let scripts: Vec<_> = entries(&output)
        .into_iter()
        .filter(|(n, _)| n.ends_with(".ps1"))
        .collect();
    assert_eq!(
        scripts,
        [(
            "tools/ChocolateyInstall.ps1".to_string(),
            "Write-Host internal".to_string()
        )]
    );

    nupkg(&input, None);
    with_script(&input, &output, b"Write-Host added".to_vec())
        .await
        .unwrap();
    assert!(entries(&output).contains(&(
        "tools/chocolateyInstall.ps1".to_string(),
        "Write-Host added".to_string()
    )));
}
