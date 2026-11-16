//! The symbol server end to end: `.snupkg` push and PDB download.

mod common;

use common::*;

// ---------------------------------------------------------------------------
// Symbol server
// ---------------------------------------------------------------------------

#[tokio::test]
async fn symbol_push_and_download_roundtrip() {
    let server = spawn().await;

    let guid: [u8; 16] = [
        0xF6, 0x72, 0x7B, 0x49, 0x0A, 0x39, 0xFC, 0x44, 0x87, 0x8E, 0x5A, 0x2D, 0x63, 0xB6, 0xCC,
        0x4B,
    ];
    let pdb = build_portable_pdb(&guid);
    let key = yanuget::pdb::portable_pdb_signature(&pdb).expect("portable pdb key");

    // The owning package — with the assembly this PDB belongs to — must exist
    // before symbols can be pushed.
    push_multipart(
        &server,
        API_KEY,
        build_nupkg_for_pdb("Sym.Lib", "1.0.0", "sym.lib.pdb", &pdb),
    )
    .await;
    let snupkg = build_snupkg("Sym.Lib", "1.0.0", "sym.lib.pdb", &pdb);

    let resp = push_symbol(&server, API_KEY, snupkg).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // The debugger fetches it over the SSQP path.
    let url = format!("/download/symbols/sym.lib.pdb/{key}/sym.lib.pdb");
    let resp = server.client.get(server.url(&url)).send().await.unwrap();
    assert!(resp.status().is_success(), "symbol download failed: {url}");
    let body = resp.bytes().await.unwrap();
    assert_eq!(&body[..], &pdb[..]);

    // An unknown key yields 404 so debuggers move to the next source.
    let miss = server
        .client
        .get(server.url("/download/symbols/sym.lib.pdb/DEADBEEFFFFFFFFF/sym.lib.pdb"))
        .send()
        .await
        .unwrap();
    assert_eq!(miss.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn symbol_push_requires_existing_package() {
    let server = spawn().await;
    let pdb = build_portable_pdb(&[1u8; 16]);
    let snupkg = build_snupkg("Ghost.Pkg", "9.9.9", "ghost.pdb", &pdb);
    let resp = push_symbol(&server, API_KEY, snupkg).await;
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn service_index_advertises_symbol_server() {
    let server = spawn().await;
    let body: serde_json::Value = server
        .client
        .get(server.url("/v3/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let types: Vec<&str> = body["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["@type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"SymbolServer/4.9.0"));
    assert!(types.contains(&"SymbolPackagePublish/4.9.0"));
}

/// `COR-47`: two versions built from the same assemblies push the same PDB.
/// The second push finds the bytes in place and records nothing, so the key
/// stayed with the first version, and deleting that one took the second
/// one's symbols with it. The key is handed over instead.
#[tokio::test]
async fn symbols_shared_by_two_versions_outlive_the_first() {
    let server = spawn_with(|c| c.hard_delete_enabled = true).await;
    let pdb = build_portable_pdb(&[0x5a; 16]);
    let key = yanuget::pdb::portable_pdb_signature(&pdb).expect("portable pdb key");
    for version in ["1.0.0-rc.1", "1.0.0"] {
        push_multipart(
            &server,
            API_KEY,
            build_nupkg_for_pdb("Twice.Lib", version, "twice.lib.pdb", &pdb),
        )
        .await;
        let snupkg = build_snupkg("Twice.Lib", version, "twice.lib.pdb", &pdb);
        let resp = push_symbol(&server, API_KEY, snupkg).await;
        assert_eq!(resp.status(), reqwest::StatusCode::CREATED, "{version}");
    }
    // A version without symbols of its own is no one to hand them to.
    push_multipart(
        &server,
        API_KEY,
        build_nupkg_for_pdb("Twice.Lib", "0.9.0", "twice.lib.pdb", &pdb),
    )
    .await;

    let url = server.url(&format!(
        "/download/symbols/twice.lib.pdb/{key}/twice.lib.pdb"
    ));
    let delete = |version: &'static str| {
        server
            .client
            .delete(server.url(&format!("/api/v2/package/twice.lib/{version}")))
            .header("X-NuGet-ApiKey", API_KEY)
            .send()
    };
    let resp = delete("1.0.0-rc.1").await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    let resp = server.client.get(&url).send().await.unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "1.0.0 lost its symbols"
    );
    assert_eq!(&resp.bytes().await.unwrap()[..], &pdb[..]);

    // With the last version that pushed it, the PDB goes.
    let resp = delete("1.0.0").await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    let resp = server.client.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}
