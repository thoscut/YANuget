//! The NuGet v3 reader takes JSON from upstream servers nobody here controls.
//! Whatever shape it gets, reading it must come back as a value or an error,
//! never as a panic: these generate JSON from the keys the reader looks for,
//! nested and typed at random, and read every document type from it.

use proptest::prelude::*;
use serde_json::{Map, Value};
use url::Url;
use yanuget_pkg::v3::{
    Catalog, CatalogEntry, PackageVersions, RegistrationIndex, RegistrationPage, SearchPage,
    ServiceIndex,
};

const KEYS: &[&str] = &[
    "@id",
    "@type",
    "items",
    "resources",
    "catalogEntry",
    "packageContent",
    "id",
    "version",
    "versions",
    "listed",
    "published",
    "packageHash",
    "packageHashAlgorithm",
    "packageSize",
    "dependencyGroups",
    "dependencies",
    "range",
    "targetFramework",
    "data",
    "totalHits",
    "commitTimeStamp",
    "nuget:id",
    "nuget:version",
];

fn scalar() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::from),
        any::<f64>()
            .prop_map(|f| serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)),
        prop_oneof![
            Just(String::new()),
            Just("1900-01-01T00:00:00Z".to_owned()),
            Just("../relative/page.json".to_owned()),
            Just("https://example.test/v3/index.json".to_owned()),
            Just("RegistrationsBaseUrl/3.6.0".to_owned()),
            Just("1.0.0-beta.1+meta".to_owned()),
            "\\PC{0,12}",
        ]
        .prop_map(Value::String),
    ]
}

fn json() -> impl Strategy<Value = Value> {
    scalar().prop_recursive(5, 64, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::vec((prop::sample::select(KEYS), inner), 0..8).prop_map(|pairs| {
                Value::Object(
                    pairs
                        .into_iter()
                        .map(|(k, v)| (k.to_owned(), v))
                        .collect::<Map<_, _>>(),
                )
            }),
        ]
    })
}

fn read_all(bytes: &[u8]) {
    let base = Url::parse("https://example.test/v3/").unwrap();
    if let Ok(index) = serde_json::from_slice::<ServiceIndex>(bytes) {
        let _ = index.find(&["RegistrationsBaseUrl/3.6.0", "SearchQueryService"]);
        let _ = index.resolve(&["RegistrationsBaseUrl/3.6.0"], &base);
    }
    if let Ok(registration) = serde_json::from_slice::<RegistrationIndex>(bytes) {
        for page in registration.items.iter().flatten() {
            for leaf in page.items.iter().flatten() {
                let entry = leaf.entry();
                let _ = entry.is_listed();
                let _ = entry.dependencies().count();
            }
        }
    }
    let _ = serde_json::from_slice::<RegistrationPage>(bytes);
    if let Ok(entry) = serde_json::from_slice::<CatalogEntry>(bytes) {
        let _ = entry.is_listed();
        let _ = entry.dependencies().count();
    }
    let _ = serde_json::from_slice::<PackageVersions>(bytes);
    let _ = serde_json::from_slice::<SearchPage>(bytes);
    let _ = serde_json::from_slice::<Catalog>(bytes);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn any_json_reads_without_a_panic(value in json()) {
        read_all(&serde_json::to_vec(&value).unwrap());
    }

    #[test]
    fn any_bytes_read_without_a_panic(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
        read_all(&bytes);
    }
}
