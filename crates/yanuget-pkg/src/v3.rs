//! The NuGet v3 documents an upstream serves: the service index, the
//! registration (index, pages, leaves), the flat container's version list,
//! search pages and the catalog.
//!
//! Upstreams differ in what they send, so the reading is lenient: unknown
//! fields are ignored, and a field of the wrong shape reads as absent rather
//! than failing the document. A list is `None` when the field is missing or
//! not a list, so a reader can tell that apart from an empty one. Nothing is
//! read into an intermediate `serde_json::Value`.

use std::fmt;
use std::marker::PhantomData;

use serde::de::value::MapAccessDeserializer;
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use url::Url;

/// The service index (`index.json`): the resources a feed offers.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ServiceIndex {
    #[serde(default, deserialize_with = "objects")]
    pub resources: Option<Vec<Resource>>,
}

impl ServiceIndex {
    /// The `@id` of the first resource of the first of `types` the index
    /// has, as written. `types` is in order of preference.
    pub fn find(&self, types: &[&str]) -> Option<&str> {
        let list = self.resources.as_deref().unwrap_or_default();
        types.iter().find_map(|t| {
            list.iter()
                .filter(|r| r.has_type(t))
                .find_map(|r| r.id.as_deref())
        })
    }

    /// [`Self::find`], resolved against `base` (the index's own URL), so a
    /// relative `@id` works. A type whose `@id` does not resolve gives way
    /// to the next.
    pub fn resolve(&self, types: &[&str], base: &Url) -> Option<Url> {
        let list = self.resources.as_deref().unwrap_or_default();
        types.iter().find_map(|t| {
            list.iter()
                .filter(|r| r.has_type(t))
                .find_map(|r| r.id.as_deref())
                .and_then(|id| base.join(id).ok())
        })
    }
}

/// One resource of the service index.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Resource {
    #[serde(rename = "@id", default, deserialize_with = "scalar")]
    pub id: Option<String>,
    /// `@type`, which a feed may send as one string or as a list of them.
    #[serde(rename = "@type", default, deserialize_with = "types")]
    pub types: Vec<String>,
}

impl Resource {
    pub fn has_type(&self, ty: &str) -> bool {
        self.types.iter().any(|t| t == ty)
    }
}

/// A package's registration index: its pages.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RegistrationIndex {
    #[serde(default, deserialize_with = "objects")]
    pub items: Option<Vec<RegistrationPage>>,
}

/// One registration page. Small packages inline its leaves (`items`);
/// large ones leave it to be fetched from `@id`, which answers with the same
/// shape.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RegistrationPage {
    #[serde(rename = "@id", default, deserialize_with = "scalar")]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "objects")]
    pub items: Option<Vec<RegistrationLeaf>>,
}

/// One version in a registration page.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RegistrationLeaf {
    #[serde(rename = "catalogEntry", default, deserialize_with = "object")]
    pub catalog_entry: Option<CatalogEntry>,
    #[serde(rename = "packageContent", default, deserialize_with = "scalar")]
    pub package_content: Option<String>,
    /// The leaf's own fields read as a catalog entry, for a server that
    /// puts them there instead of under `catalogEntry`. Only the leaf's few
    /// other fields are buffered for it.
    #[serde(flatten)]
    inline: CatalogEntry,
}

impl RegistrationLeaf {
    /// The catalog entry, or the leaf itself when it has none.
    pub fn entry(&self) -> &CatalogEntry {
        self.catalog_entry.as_ref().unwrap_or(&self.inline)
    }
}

/// What a registration leaf says about its version.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    #[serde(default, deserialize_with = "scalar")]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "scalar")]
    pub version: Option<String>,
    #[serde(default, deserialize_with = "scalar")]
    pub listed: Option<bool>,
    #[serde(default, deserialize_with = "scalar")]
    pub published: Option<String>,
    #[serde(default, deserialize_with = "scalar")]
    pub package_content: Option<String>,
    #[serde(default, deserialize_with = "scalar")]
    pub package_hash: Option<String>,
    #[serde(default, deserialize_with = "scalar")]
    pub package_hash_algorithm: Option<String>,
    #[serde(default, deserialize_with = "scalar")]
    pub package_size: Option<u64>,
    #[serde(default, deserialize_with = "objects")]
    pub dependency_groups: Option<Vec<DependencyGroup>>,
}

impl CatalogEntry {
    /// Whether the version is listed: `listed: false` is the flag, and a
    /// `published` date in 1900 is how older servers said the same thing.
    pub fn is_listed(&self) -> bool {
        self.listed != Some(false)
            && !self
                .published
                .as_deref()
                .is_some_and(|p| p.starts_with("1900-"))
    }

    /// Every dependency of every group, in order.
    pub fn dependencies(&self) -> impl Iterator<Item = &Dependency> {
        self.dependency_groups
            .iter()
            .flatten()
            .flat_map(|g| g.dependencies.iter().flatten())
    }
}

/// The dependencies for one target framework.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyGroup {
    #[serde(default, deserialize_with = "scalar")]
    pub target_framework: Option<String>,
    #[serde(default, deserialize_with = "objects")]
    pub dependencies: Option<Vec<Dependency>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Dependency {
    #[serde(default, deserialize_with = "scalar")]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "scalar")]
    pub range: Option<String>,
}

/// The flat container's version list (`{id}/index.json`). Entries that are
/// not strings are dropped.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PackageVersions {
    #[serde(default, deserialize_with = "strings")]
    pub versions: Option<Vec<String>>,
}

/// One page of `SearchQueryService` results.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchPage {
    #[serde(default, deserialize_with = "objects")]
    pub data: Option<Vec<SearchResult>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchResult {
    #[serde(default, deserialize_with = "scalar")]
    pub id: Option<String>,
}

/// A catalog index or page: both list `items`, the index its pages (by
/// `@id`) and a page its package details (by `nuget:id`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Catalog {
    #[serde(default, deserialize_with = "objects")]
    pub items: Option<Vec<CatalogItem>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CatalogItem {
    #[serde(rename = "@id", default, deserialize_with = "scalar")]
    pub id: Option<String>,
    #[serde(rename = "nuget:id", default, deserialize_with = "scalar")]
    pub package_id: Option<String>,
}

// The lenient readers. Objects and lists are visited in place, so a large
// document is never buffered; only a scalar field is, to try it as `T`.

/// A value of type `T`, or anything else read as absent.
#[derive(Deserialize)]
#[serde(untagged)]
enum Or<T> {
    Value(T),
    Other(IgnoredAny),
}

impl<T> Or<T> {
    fn get(self) -> Option<T> {
        match self {
            Or::Value(v) => Some(v),
            Or::Other(_) => None,
        }
    }
}

fn scalar<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    Ok(Or::<T>::deserialize(d)?.get())
}

/// `@type`: one string, or a list of them.
fn types<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Types {
        One(String),
        Many(Vec<Or<String>>),
        Other(IgnoredAny),
    }
    Ok(match Types::deserialize(d)? {
        Types::One(t) => vec![t],
        Types::Many(list) => list.into_iter().filter_map(Or::get).collect(),
        Types::Other(_) => Vec::new(),
    })
}

fn object<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    Ok(Object::<T>::deserialize(d)?.0)
}

/// A list, keeping the entries that are objects.
fn objects<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<Vec<T>>, D::Error> {
    Ok(Array::<Object<T>>::deserialize(d)?
        .0
        .map(|list| list.into_iter().filter_map(|o| o.0).collect()))
}

/// A list, keeping the entries that are strings.
fn strings<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    Ok(Array::<Or<String>>::deserialize(d)?
        .0
        .map(|list| list.into_iter().filter_map(Or::get).collect()))
}

/// `T` read from an object; `None` for any other value.
struct Object<T>(Option<T>);

/// A list of `T`; `None` for any other value.
struct Array<T>(Option<Vec<T>>);

/// Answers `None` for every value but the one shape the wrapper wants.
macro_rules! absent_otherwise {
    () => {
        fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
            Ok(Self::Value::default())
        }
        fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
            Ok(Self::Value::default())
        }
        fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
            Ok(Self::Value::default())
        }
        fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
            Ok(Self::Value::default())
        }
        fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
            Ok(Self::Value::default())
        }
        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(Self::Value::default())
        }
        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(Self::Value::default())
        }
    };
}

impl<T> Default for Object<T> {
    fn default() -> Self {
        Object(None)
    }
}

impl<T> Default for Array<T> {
    fn default() -> Self {
        Array(None)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = Object<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any value")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(|v| Object(Some(v)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Object(None))
            }
            absent_otherwise!();
        }
        d.deserialize_any(V(PhantomData))
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Array<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = Array<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any value")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut list = Vec::new();
                while let Some(item) = seq.next_element()? {
                    list.push(item);
                }
                Ok(Array(Some(list)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Array(None))
            }
            absent_otherwise!();
        }
        d.deserialize_any(V(PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read<T: for<'de> Deserialize<'de>>(v: serde_json::Value) -> T {
        // From text, as an upstream sends it, rather than from a `Value`.
        serde_json::from_str(&v.to_string()).unwrap()
    }

    #[test]
    fn the_service_index_takes_type_as_a_string_or_a_list() {
        let index: ServiceIndex = read(json!({
            "version": "3.0.0",
            "resources": [
                {"@id": "https://feed.example/v3/registration/", "@type": "RegistrationsBaseUrl"},
                {"@id": "https://feed.example/v3/semver2/", "@type": ["RegistrationsBaseUrl/3.6.0", 7]},
                {"@id": 5, "@type": "PackageBaseAddress/3.0.0"},
                {"@id": "/v3/flat", "@type": "PackageBaseAddress/3.0.0", "comment": "x"},
                {"@type": {"odd": true}},
                "not a resource"
            ]
        }));
        assert_eq!(index.resources.as_ref().unwrap().len(), 5);
        assert_eq!(
            index.find(&["RegistrationsBaseUrl/3.6.0", "RegistrationsBaseUrl"]),
            Some("https://feed.example/v3/semver2/")
        );
        // A resource without a usable `@id` gives way to the next of its type.
        assert_eq!(index.find(&["PackageBaseAddress/3.0.0"]), Some("/v3/flat"));
        assert_eq!(index.find(&["Catalog/3.0.0"]), None);
    }

    #[test]
    fn a_relative_id_resolves_against_the_index() {
        let base = Url::parse("https://feed.example/v3/index.json").unwrap();
        let index: ServiceIndex = read(json!({"resources": [
            {"@id": "flat/", "@type": "PackageBaseAddress/3.0.0"},
            {"@id": "/other/query", "@type": ["SearchQueryService"]},
            {"@id": "https://elsewhere.example/reg/", "@type": "RegistrationsBaseUrl"},
        ]}));
        let at = |t: &str| index.resolve(&[t], &base).map(|u| u.to_string());
        assert_eq!(
            at("PackageBaseAddress/3.0.0").as_deref(),
            Some("https://feed.example/v3/flat/")
        );
        assert_eq!(
            at("SearchQueryService").as_deref(),
            Some("https://feed.example/other/query")
        );
        assert_eq!(
            at("RegistrationsBaseUrl").as_deref(),
            Some("https://elsewhere.example/reg/")
        );
    }

    #[test]
    fn missing_or_odd_lists_read_as_absent() {
        let index: ServiceIndex = read(json!({}));
        assert!(index.resources.is_none());
        assert_eq!(index.find(&["PackageBaseAddress/3.0.0"]), None);
        let index: ServiceIndex = read(json!({"resources": {"@id": "x"}}));
        assert!(index.resources.is_none());
        let index: ServiceIndex = read(json!({"resources": []}));
        assert_eq!(index.resources.unwrap().len(), 0);
        let versions: PackageVersions = read(json!({"versions": ["1.0.0", 2, null, "2.0.0"]}));
        assert_eq!(versions.versions.unwrap(), ["1.0.0", "2.0.0"]);
        let versions: PackageVersions = read(json!({"versions": "1.0.0"}));
        assert!(versions.versions.is_none());
    }

    #[test]
    fn registration_pages_and_leaves_parse() {
        let index: RegistrationIndex = read(json!({
            "count": 2,
            "items": [
                {"@id": "https://feed.example/reg/git/page/0.json", "lower": "1.0.0"},
                {"@id": "https://feed.example/reg/git/index.json#page/2", "items": [
                    {
                        "@id": "https://feed.example/reg/git/2.51.0.json",
                        "packageContent": "https://feed.example/flat/git/2.51.0/git.2.51.0.nupkg",
                        "catalogEntry": {
                            "id": "Git", "version": "2.51.0", "listed": false,
                            "packageSize": 1234,
                            "dependencyGroups": [
                                {"dependencies": [{"id": "git.install", "range": "[2.51.0]"}]},
                                {"targetFramework": "net45", "dependencies": null},
                                {"targetFramework": "net48"}
                            ],
                            "tags": ["git"]
                        }
                    },
                    {"catalogEntry": {"id": "Git", "version": "2.50.0", "published": "1900-01-01T00:00:00Z"}},
                    {"catalogEntry": "https://feed.example/catalog/git.2.49.0.json"},
                    {"version": "2.48.0", "listed": true}
                ]}
            ]
        }));
        let pages = index.items.unwrap();
        assert_eq!(pages.len(), 2);
        assert!(pages[0].items.is_none());
        assert_eq!(
            pages[0].id.as_deref(),
            Some("https://feed.example/reg/git/page/0.json")
        );
        let leaves = pages[1].items.as_ref().unwrap();
        assert_eq!(leaves.len(), 4);

        let first = leaves[0].entry();
        assert_eq!(first.id.as_deref(), Some("Git"));
        assert!(!first.is_listed());
        assert_eq!(first.package_size, Some(1234));
        let deps: Vec<_> = first.dependencies().collect();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].id.as_deref(), Some("git.install"));
        assert_eq!(deps[0].range.as_deref(), Some("[2.51.0]"));
        assert!(leaves[0].package_content.is_some());

        // A 1900 publication date is the old way to unlist.
        assert!(!leaves[1].entry().is_listed());
        // A catalog entry by reference only says nothing here.
        assert!(leaves[2].catalog_entry.is_none());
        assert_eq!(leaves[2].entry().version, None);
        // Without a catalog entry, the leaf's own fields stand in.
        assert!(leaves[3].catalog_entry.is_none());
        assert_eq!(leaves[3].entry().version.as_deref(), Some("2.48.0"));
        assert!(leaves[3].entry().is_listed());
    }

    #[test]
    fn search_and_catalog_pages_parse() {
        let page: SearchPage = read(json!({"totalHits": 2, "data": [
            {"id": "Alpha", "version": "1.0.0"}, {"version": "9.9.9"}
        ]}));
        let data = page.data.unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0].id.as_deref(), Some("Alpha"));
        assert!(data[1].id.is_none());

        let catalog: Catalog = read(json!({"items": [
            {"@id": "https://feed.example/catalog/page0.json", "count": 1},
            {"@id": "https://feed.example/catalog/x.json", "nuget:id": "Alpha"}
        ]}));
        let items = catalog.items.unwrap();
        assert_eq!(items[0].package_id, None);
        assert_eq!(items[1].package_id.as_deref(), Some("Alpha"));
    }
}
