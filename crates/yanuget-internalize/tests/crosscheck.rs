//! The static cross-check over a real package: which entries are read, and
//! what the scan finds in them.

use std::io::Write;

use yanuget_internalize::crosscheck::{scan, Mention, MAX_SCRIPT_BYTES};

#[tokio::test]
async fn every_script_in_the_package_is_scanned() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tool.2.0.0.nupkg");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    let mut add = |name: &str, bytes: &[u8]| {
        zip.start_file(name, opts).unwrap();
        zip.write_all(bytes).unwrap();
    };
    add(
        "tool.nuspec",
        b"<package><metadata><projectUrl>https://example.com/project</projectUrl></metadata></package>",
    );
    add(
        "tools/chocolateyInstall.ps1",
        b"Install-ChocolateyPackage tool exe '/S' 'https://example.com/2.0/setup.exe'",
    );
    // Windows PowerShell's UTF-16 with a byte-order mark.
    let mut uninstall = vec![0xFF, 0xFE];
    uninstall.extend(
        "# cleanup\r\n$u = \"https://example.com/cleanup.exe\"\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes),
    );
    add("tools/chocolateyUninstall.ps1", &uninstall);
    add(
        "tools\\Helpers.PSM1",
        b"function Get-Url { 'https://example.com/helper.zip' }",
    );
    add("tools/readme.txt", b"https://example.com/not-a-script");
    add(
        "internalizer/prelude.ps1",
        b"$base = 'https://nuget.example.com/files/'",
    );
    let mut huge = b"# padding\n".repeat((MAX_SCRIPT_BYTES / 10 + 1) as usize);
    huge.extend_from_slice(b"'https://example.com/hidden.exe'");
    add("tools/huge.ps1", &huge);
    zip.finish().unwrap();

    let found = scan(&path).await.unwrap();
    let mention = |entry: &str, line, url: &str| Mention {
        entry: entry.into(),
        line,
        url: url.into(),
    };
    assert_eq!(
        found.mentions,
        [
            mention(
                "tools/chocolateyInstall.ps1",
                1,
                "https://example.com/2.0/setup.exe"
            ),
            mention(
                "tools/chocolateyUninstall.ps1",
                2,
                "https://example.com/cleanup.exe"
            ),
            mention("tools\\Helpers.PSM1", 1, "https://example.com/helper.zip"),
        ]
    );
    assert_eq!(found.skipped, ["tools/huge.ps1"]);

    std::fs::write(&path, b"not a zip").unwrap();
    assert!(scan(&path).await.is_err());
}
