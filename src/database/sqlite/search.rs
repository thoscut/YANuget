//! Search, autocomplete and the tag cloud: the FTS5 index (`search_text`,
//! `search_keys`) and `package_tags`, scoped to a feed.

use sqlx::Row;

use crate::database::{canonical_id, SearchGroup, SearchPage, SearchRequest, SearchSort, TagCount};
use crate::error::Result;
use crate::models::Package;

use super::packages::row_to_feed_package;
use super::{placeholders, SqliteDatabase, ID_CHUNK};

pub(super) async fn search(
    db: &SqliteDatabase,
    feed: &str,
    request: &SearchRequest,
) -> Result<SearchPage> {
    let query = search_terms(&request.query);
    // Two or more characters... three, in fact: a trigram index can only
    // answer a query that has at least one trigram. Shorter ones scan the
    // (capped) indexed text instead, which is cheap for what they are.
    let short = query.chars().count() < 3;
    let needle = if short {
        like_pattern(&query)
    } else {
        // An FTS5 phrase: the whole query, quotes doubled, matched as a
        // substring of any indexed column.
        format!("\"{}\"", query.replace('"', "\"\""))
    };

    // An optional package-type filter, applied in SQL so the page and the
    // total count stay consistent. `''` (no filter) makes the predicate a
    // no-op; otherwise a package id matches when any of its versions
    // declares the type. `json_each`/`json_extract` parse the stored JSON
    // so there is no quoting/escaping ambiguity.
    let package_type = request.package_type.as_deref().unwrap_or("").to_lowercase();
    // An optional tag, matched exactly (case-insensitively) against the
    // tag index: a package matches when any of its visible versions has it.
    let tag = request.tag.as_deref().unwrap_or("").trim().to_lowercase();

    // Phase 1: pick the page of matching package ids, ranked by downloads.
    //
    // `$how` names the `matcher!` that selects the rowids of the matching
    // FTS rows (`?5`).
    macro_rules! filter {
        ($how:ident) => {
            concat!(
                "fp.feed = ?1 AND fp.listed = 1 AND fp.enabled = 1 AND fp.pending = 0 \
             AND (?2 = 1 OR p.is_prerelease = 0) \
             AND (?3 = 1 OR p.is_semver2 = 0) \
             AND (?4 = '' OR (p.lower_id, p.normalized_version) IN ( \
                  SELECT k.lower_id, k.normalized_version FROM search_keys k \
                  WHERE k.id IN (",
                matcher!($how),
                "))) \
             AND (?6 = '' OR EXISTS ( \
                  SELECT 1 FROM json_each(p.package_types) je \
                  WHERE lower(json_extract(je.value, '$.name')) = ?6)) \
             AND (?7 = '' OR EXISTS ( \
                  SELECT 1 FROM package_tags pt \
                  WHERE pt.lower_id = p.lower_id \
                    AND pt.normalized_version = p.normalized_version AND pt.tag = ?7))"
            )
        };
    }
    macro_rules! matcher {
        (phrase) => {
            "SELECT rowid FROM search_text WHERE search_text MATCH ?5"
        };
        (scan) => {
            "SELECT rowid FROM search_text WHERE lower_id LIKE ?5 ESCAPE '\\' \
             OR title LIKE ?5 ESCAPE '\\' OR tags LIKE ?5 ESCAPE '\\' \
             OR description LIKE ?5 ESCAPE '\\'"
        };
    }

    // One statement per order, each still a compile-time constant. The id
    // is the last key of every order, so a page boundary never falls
    // between two packages that tie.
    macro_rules! page_of_ids {
        ($order:literal, $how:ident) => {
            concat!(
                "SELECT p.lower_id AS lower_id, SUM(fp.downloads) AS total, \
                 MAX(p.published) AS updated \
                 FROM packages p JOIN feed_packages fp \
                   ON fp.lower_id = p.lower_id AND fp.normalized_version = p.normalized_version \
                 WHERE ",
                filter!($how),
                " GROUP BY p.lower_id ORDER BY ",
                $order,
                " LIMIT ?8 OFFSET ?9"
            )
        };
    }
    // `published` is stored as RFC 3339 in UTC, so it orders as text.
    let page_sql = match (request.sort, short) {
        (SearchSort::Downloads, false) => page_of_ids!("total DESC, p.lower_id ASC", phrase),
        (SearchSort::Downloads, true) => page_of_ids!("total DESC, p.lower_id ASC", scan),
        (SearchSort::Name, false) => page_of_ids!("p.lower_id ASC", phrase),
        (SearchSort::Name, true) => page_of_ids!("p.lower_id ASC", scan),
        (SearchSort::Updated, false) => {
            page_of_ids!("updated DESC, p.lower_id ASC", phrase)
        }
        (SearchSort::Updated, true) => page_of_ids!("updated DESC, p.lower_id ASC", scan),
    };

    let id_rows = sqlx::query(page_sql)
        .bind(feed)
        .bind(i64::from(request.include_prerelease))
        .bind(i64::from(request.include_semver2))
        .bind(&query)
        .bind(&needle)
        .bind(&package_type)
        .bind(&tag)
        .bind(request.take.max(0))
        .bind(request.skip.max(0))
        .fetch_all(&db.pool)
        .await?;

    let ids: Vec<String> = id_rows
        .iter()
        .map(|r| r.get::<String, _>("lower_id"))
        .collect();

    macro_rules! count {
        ($how:ident) => {
            concat!(
                "SELECT COUNT(*) FROM ( \
                     SELECT p.lower_id FROM packages p JOIN feed_packages fp \
                       ON fp.lower_id = p.lower_id \
                      AND fp.normalized_version = p.normalized_version \
                     WHERE ",
                filter!($how),
                " GROUP BY p.lower_id )"
            )
        };
    }
    let count_sql = if short { count!(scan) } else { count!(phrase) };
    let total_hits: i64 = sqlx::query_scalar(count_sql)
        .bind(feed)
        .bind(i64::from(request.include_prerelease))
        .bind(i64::from(request.include_semver2))
        .bind(&query)
        .bind(&needle)
        .bind(&package_type)
        .bind(&tag)
        .fetch_one(&db.pool)
        .await?;

    // Phase 2: load every visible version for the chosen ids.
    let groups = load_groups(
        db,
        feed,
        &ids,
        request.include_prerelease,
        request.include_semver2,
        true,
    )
    .await?;

    Ok(SearchPage { total_hits, groups })
}

pub(super) async fn autocomplete(
    db: &SqliteDatabase,
    feed: &str,
    query: &str,
    include_prerelease: bool,
    include_semver2: bool,
    skip: i64,
    take: i64,
) -> Result<(Vec<String>, i64)> {
    let q = canonical_id(&search_terms(query));
    let pattern = like_pattern(&q);
    // The version predicates sit inside the grouped scan, so an id survives
    // only if it still has at least one version the caller would accept.
    macro_rules! matching_ids {
        () => {
            r#"
        FROM packages p JOIN feed_packages fp
            ON fp.lower_id = p.lower_id AND fp.normalized_version = p.normalized_version
        WHERE fp.feed = ?1 AND fp.listed = 1 AND fp.enabled = 1 AND fp.pending = 0
          AND (?2 = '' OR p.lower_id LIKE ?3 ESCAPE '\')
          AND (?4 = 1 OR p.is_prerelease = 0)
          AND (?5 = 1 OR p.is_semver2 = 0)
        GROUP BY p.lower_id"#
        };
    }

    let rows = sqlx::query(concat!(
        "SELECT MAX(p.id) AS id ",
        matching_ids!(),
        " ORDER BY p.lower_id ASC LIMIT ?6 OFFSET ?7"
    ))
    .bind(feed)
    .bind(&q)
    .bind(&pattern)
    .bind(i64::from(include_prerelease))
    .bind(i64::from(include_semver2))
    .bind(take.max(0))
    .bind(skip.max(0))
    .fetch_all(&db.pool)
    .await?;
    let ids: Vec<String> = rows.iter().map(|r| r.get::<String, _>("id")).collect();

    // `GROUP BY` makes this a count of groups, not of rows, so it has to be
    // wrapped rather than written as a bare `COUNT(*)`.
    let total: i64 = sqlx::query_scalar(concat!(
        "SELECT COUNT(*) FROM (SELECT p.lower_id ",
        matching_ids!(),
        ")"
    ))
    .bind(feed)
    .bind(&q)
    .bind(&pattern)
    .bind(i64::from(include_prerelease))
    .bind(i64::from(include_semver2))
    .fetch_one(&db.pool)
    .await?;

    Ok((ids, total))
}

pub(super) async fn tag_counts(
    db: &SqliteDatabase,
    feed: &str,
    limit: i64,
) -> Result<Vec<TagCount>> {
    // The same visibility as search, so every tag listed leads somewhere.
    let rows = sqlx::query(
        "SELECT pt.tag AS tag, COUNT(DISTINCT pt.lower_id) AS packages \
         FROM package_tags pt JOIN feed_packages fp \
           ON fp.lower_id = pt.lower_id AND fp.normalized_version = pt.normalized_version \
         WHERE fp.feed = ?1 AND fp.listed = 1 AND fp.enabled = 1 AND fp.pending = 0 \
         GROUP BY pt.tag ORDER BY packages DESC, pt.tag ASC LIMIT ?2",
    )
    .bind(feed)
    .bind(limit.max(0))
    .fetch_all(&db.pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| TagCount {
            tag: r.get("tag"),
            packages: r.get("packages"),
        })
        .collect())
}

/// Load every visible version for a set of lower-cased ids in `feed`,
/// grouped and version-sorted, preserving the order of `ids`.
pub(super) async fn load_groups(
    db: &SqliteDatabase,
    feed: &str,
    ids: &[String],
    include_prerelease: bool,
    include_semver2: bool,
    listed_only: bool,
) -> Result<Vec<SearchGroup>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    // One query per chunk rather than one per id. This ran once for every
    // id on the page — twenty statements for a default gallery view, and up
    // to a thousand at `take=1000`, against a sixteen-connection pool, on a
    // page anyone can request.
    // Chunked: see `ID_CHUNK`.
    let mut by_id: std::collections::HashMap<String, Vec<Package>> =
        std::collections::HashMap::with_capacity(ids.len());
    let mut totals: std::collections::HashMap<String, u64> =
        std::collections::HashMap::with_capacity(ids.len());

    for chunk in ids.chunks(ID_CHUNK) {
        // Parameters ?1..?4 are the flags; the ids follow from ?5.
        let placeholders = placeholders(5, chunk.len());
        let sql = format!(
            concat!(
                feed_select!(),
                " WHERE fp.feed = ?1 \
                   AND fp.enabled = 1 AND fp.pending = 0 \
                   AND (?2 = 1 OR fp.listed = 1) \
                   AND (?3 = 1 OR p.is_prerelease = 0) \
                   AND (?4 = 1 OR p.is_semver2 = 0) \
                   AND fp.lower_id IN ({placeholders})"
            ),
            placeholders = placeholders
        );
        // The only interpolation is `placeholders`, which is `?5,?6,…`
        // generated from a range — the ids themselves are bound, never
        // formatted in.
        let mut query = sqlx::query(&sql)
            .bind(feed)
            .bind(i64::from(!listed_only))
            .bind(i64::from(include_prerelease))
            .bind(i64::from(include_semver2));
        for id in chunk {
            query = query.bind(id);
        }
        for row in query.fetch_all(&db.pool).await? {
            let package = row_to_feed_package(&row)?.package;
            by_id.entry(package.lower_id()).or_default().push(package);
        }

        // A package's total counts every version the feed serves, not
        // only those this search's filters admitted: `totalDownloads` is
        // the package's figure, and it used to shrink when a client left
        // out pre-releases.
        let id_params = super::placeholders(2, chunk.len());
        let sql = format!(
            "SELECT lower_id, COALESCE(SUM(downloads), 0) AS total FROM feed_packages \
             WHERE feed = ?1 AND enabled = 1 AND pending = 0 \
               AND lower_id IN ({id_params}) \
             GROUP BY lower_id"
        );
        let mut query = sqlx::query(&sql).bind(feed);
        for id in chunk {
            query = query.bind(id);
        }
        for row in query.fetch_all(&db.pool).await? {
            let total: i64 = row.try_get("total")?;
            totals.insert(row.try_get("lower_id")?, total.max(0) as u64);
        }
    }

    // Emit in the order the caller asked for — that order is the search
    // ranking, and a HashMap has none.
    let mut groups = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(mut packages) = by_id.remove(id) {
            packages.sort_by(|a, b| a.version.cmp(&b.version));
            groups.push(SearchGroup {
                packages,
                total_downloads: totals.get(id).copied().unwrap_or(0),
            });
        }
    }
    Ok(groups)
}

/// The longest search query acted on, in characters. Longer ones are cut:
/// no package id, title or tag is anywhere near it, and every character
/// makes an anonymous request's matching dearer.
pub const MAX_QUERY_CHARS: usize = 256;

/// A search or autocomplete query as matched: trimmed, lower-cased and cut at
/// [`MAX_QUERY_CHARS`].
fn search_terms(query: &str) -> String {
    query
        .trim()
        .chars()
        .take(MAX_QUERY_CHARS)
        .collect::<String>()
        .to_lowercase()
}

/// Build a `%...%` LIKE pattern, escaping the LIKE metacharacters in `query`.
fn like_pattern(query: &str) -> String {
    let mut escaped = String::with_capacity(query.len() + 2);
    escaped.push('%');
    for ch in query.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped.push('%');
    escaped
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::load_groups;

    #[tokio::test]
    async fn the_tag_filter_and_counts_see_packages_not_versions() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add_to_feed(FEED, &tagged("Log.A", "1.0.0", &["Logging", "json"]))
            .await
            .unwrap();
        db.add_to_feed(FEED, &tagged("Log.A", "1.1.0", &["logging"]))
            .await
            .unwrap();
        db.add_to_feed(FEED, &tagged("Log.B", "2.0.0", &["LOGGING"]))
            .await
            .unwrap();
        db.add_to_feed(FEED, &tagged("Other", "1.0.0", &["json"]))
            .await
            .unwrap();
        // Another feed's tags are not this feed's.
        db.add_to_feed("elsewhere", &tagged("Far", "1.0.0", &["logging", "far"]))
            .await
            .unwrap();

        let by_tag = |tag: &str| SearchRequest {
            tag: Some(tag.into()),
            ..Default::default()
        };
        let page = db.search(FEED, &by_tag("Logging")).await.unwrap();
        assert_eq!(page.total_hits, 2);
        let ids: Vec<_> = page.groups.iter().map(|g| g.latest().id.clone()).collect();
        assert!(ids.contains(&"Log.A".to_string()) && ids.contains(&"Log.B".to_string()));
        assert_eq!(db.search(FEED, &by_tag("far")).await.unwrap().total_hits, 0);
        // It narrows a search rather than replacing it.
        let page = db
            .search(
                FEED,
                &SearchRequest {
                    query: "other".into(),
                    tag: Some("json".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total_hits, 1);

        let counts = db.tag_counts(FEED, 10).await.unwrap();
        assert_eq!(
            counts,
            vec![
                TagCount {
                    tag: "json".into(),
                    packages: 2
                },
                TagCount {
                    tag: "logging".into(),
                    packages: 2
                },
            ]
        );
        assert_eq!(db.tag_counts(FEED, 1).await.unwrap().len(), 1);

        // Deleting a version takes its tags with it.
        let v = NuGetVersion::parse("2.0.0").unwrap();
        db.delete_package_data("Log.B", &v).await.unwrap();
        assert_eq!(
            db.search(FEED, &by_tag("logging"))
                .await
                .unwrap()
                .total_hits,
            1
        );
    }

    #[tokio::test]
    async fn search_matches_substrings_from_the_index() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let mut long = tagged("Long.Text", "1.0.0", &["Parsing", "json"]);
        long.title = Some("The Title Here".into());
        // Past the indexed 4000 characters: stored, served, not searched.
        long.description = format!("{} needle-at-the-end", "x".repeat(5000));
        db.add_to_feed(FEED, &long).await.unwrap();
        db.add_to_feed(FEED, &tagged("Other.Pkg", "1.0.0", &["tools"]))
            .await
            .unwrap();

        assert_eq!(hits(&db, "ng.te").await, ["Long.Text"], "id substring");
        assert_eq!(
            hits(&db, "TITLE HE").await,
            ["Long.Text"],
            "title, any case"
        );
        assert_eq!(hits(&db, "arsin").await, ["Long.Text"], "a tag");
        assert_eq!(hits(&db, "description for oth").await, ["Other.Pkg"]);
        assert!(hits(&db, "needle-at-the-end").await.is_empty());
        let stored = db
            .find(FEED, "long.text", &NuGetVersion::parse("1.0.0").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.description.len(), 5018, "stored in full");

        // Short queries, and characters that mean something to FTS or LIKE.
        assert_eq!(hits(&db, "g.").await, ["Long.Text"]);
        assert_eq!(hits(&db, "x").await, ["Long.Text"]);
        assert!(hits(&db, "%").await.is_empty());
        assert!(hits(&db, "\"x").await.is_empty());
        assert!(hits(&db, "x\" OR \"o").await.is_empty());
        // The tags are words, not the JSON they are stored as.
        assert!(hits(&db, "\",\"").await.is_empty());
        // An absurd query is cut, not refused or run in full.
        assert!(hits(&db, &"xy".repeat(10_000)).await.is_empty());

        // Deleting a version takes it out of the index.
        db.delete_package_data("Other.Pkg", &NuGetVersion::parse("1.0.0").unwrap())
            .await
            .unwrap();
        assert!(hits(&db, "description for oth").await.is_empty());
    }

    #[tokio::test]
    async fn search_groups_and_ranks() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add_to_feed(FEED, &sample("Alpha.Tools", "1.0.0"))
            .await
            .unwrap();
        db.add_to_feed(FEED, &sample("Alpha.Tools", "1.1.0"))
            .await
            .unwrap();
        db.add_to_feed(FEED, &sample("Beta.Lib", "2.0.0"))
            .await
            .unwrap();
        // Give Beta.Lib more downloads so it ranks first.
        let v = NuGetVersion::parse("2.0.0").unwrap();
        for _ in 0..5 {
            db.increment_downloads(FEED, "beta.lib", &v).await.unwrap();
        }

        let page = db
            .search(
                FEED,
                &SearchRequest {
                    query: String::new(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total_hits, 2);
        assert_eq!(page.groups.len(), 2);
        assert_eq!(page.groups[0].latest().id, "Beta.Lib");
        assert_eq!(page.groups[1].packages.len(), 2); // both Alpha versions

        // Targeted query.
        let page = db
            .search(
                FEED,
                &SearchRequest {
                    query: "alpha".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total_hits, 1);
        assert_eq!(page.groups[0].latest().id, "Alpha.Tools");
    }

    #[tokio::test]
    async fn package_type_filter_keeps_count_and_page_consistent() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let mut tool = sample("Contoso.Tool", "1.0.0");
        tool.package_types = vec![PackageType {
            name: "DotnetTool".into(),
            version: None,
        }];
        db.add_to_feed(FEED, &tool).await.unwrap();
        db.add_to_feed(FEED, &sample("Contoso.Lib", "1.0.0"))
            .await
            .unwrap();

        // The filter is applied in SQL, so total_hits matches the page.
        let page = db
            .search(
                FEED,
                &SearchRequest {
                    package_type: Some("dotnettool".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total_hits, 1);
        assert_eq!(page.groups.len(), 1);
        assert_eq!(page.groups[0].latest().id, "Contoso.Tool");

        // A type nothing declares yields zero, consistently (case-insensitive).
        let none = db
            .search(
                FEED,
                &SearchRequest {
                    package_type: Some("Template".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(none.total_hits, 0);
        assert_eq!(none.groups.len(), 0);
    }

    #[tokio::test]
    async fn prerelease_filter_hides_prereleases() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add_to_feed(FEED, &sample("Only.Pre", "1.0.0-alpha"))
            .await
            .unwrap();
        let page = db
            .search(
                FEED,
                &SearchRequest {
                    include_prerelease: false,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total_hits, 0);
    }

    /// `load_groups` batches its ids into one query per chunk and reassembles
    /// the result. Two things have to survive that: the caller's order, which
    /// *is* the search ranking and which a `HashMap` does not preserve, and the
    /// version order inside each group.
    #[tokio::test]
    async fn grouped_loading_keeps_both_orderings() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        for (id, version) in [
            ("Alpha.Pkg", "1.0.0"),
            ("Alpha.Pkg", "2.0.0"),
            ("Alpha.Pkg", "1.5.0"),
            ("Beta.Pkg", "0.1.0"),
            ("Gamma.Pkg", "3.0.0"),
        ] {
            db.add_to_feed(FEED, &sample(id, version)).await.unwrap();
        }

        // Deliberately not alphabetical: this is the ranking the caller chose.
        let ids = vec![
            "gamma.pkg".to_string(),
            "alpha.pkg".to_string(),
            "beta.pkg".to_string(),
        ];
        let groups = load_groups(&db, FEED, &ids, true, true, false)
            .await
            .unwrap();

        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].latest().lower_id(), "gamma.pkg");
        assert_eq!(groups[1].latest().lower_id(), "alpha.pkg");
        assert_eq!(groups[2].latest().lower_id(), "beta.pkg");

        // Versions ascend within a group, so `latest()` really is the latest.
        let alpha: Vec<String> = groups[1]
            .packages
            .iter()
            .map(|p| p.normalized_version())
            .collect();
        assert_eq!(alpha, ["1.0.0", "1.5.0", "2.0.0"]);

        // An id with nothing visible is dropped rather than yielding an empty
        // group, which `latest()` would panic on.
        let missing = vec!["nope.pkg".to_string(), "beta.pkg".to_string()];
        let groups = load_groups(&db, FEED, &missing, true, true, false)
            .await
            .unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].latest().lower_id(), "beta.pkg");

        assert!(load_groups(&db, FEED, &[], true, true, false)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn delete_and_autocomplete() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add_to_feed(FEED, &sample("Contoso.Cli", "1.0.0"))
            .await
            .unwrap();
        db.add_to_feed(FEED, &sample("Contoso.Core", "1.0.0"))
            .await
            .unwrap();

        let (ac, total) = db
            .autocomplete(FEED, "contoso", true, true, 0, 20)
            .await
            .unwrap();
        assert_eq!(ac.len(), 2);
        assert_eq!(total, 2);
        assert!(ac.contains(&"Contoso.Cli".to_string()));

        // The total counts every match, not the page: a caller paging on it
        // must be able to reach the ids the first page left out.
        let (page, total) = db
            .autocomplete(FEED, "contoso", true, true, 0, 1)
            .await
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(total, 2, "totalHits must count matches, not the page");

        let v = NuGetVersion::parse("1.0.0").unwrap();
        assert!(db.delete_package_data("contoso.cli", &v).await.unwrap());
        assert!(!db.exists(FEED, "contoso.cli", &v).await.unwrap());
        let (ac, total) = db
            .autocomplete(FEED, "contoso", true, true, 0, 20)
            .await
            .unwrap();
        assert_eq!(ac, vec!["Contoso.Core".to_string()]);
        assert_eq!(total, 1);
    }
}
