# Gallery and admin area

YANuget has two sets of HTML pages: the **gallery**, which anyone who may read
the feed can browse, and the **admin area**, where an operator moderates
versions. Both are served by the binary itself and load nothing from outside
the server. The endpoints behind them are listed in the [HTTP API](api.md#web-gallery-html).

## The gallery

The gallery is the feed's home page (`/`, or `/{feed}/` with several feeds):

* **Package list** with a search box, sortable by downloads, name or last
  update, and paged (`gallery_page_size`, 20 by default).
* **Tags**: every tag links to the packages carrying it; `/tags` shows them all
  as a cloud.
* **Package page**: versions, dependencies, readme, links, whether symbols are
  available, a download link for the `.nupkg`, attached files, and the install
  command for Chocolatey, `dotnet` and `nuget.exe`, in the order
  `primary_client` sets.
* **Statistics** (`/stats`): totals, the most downloaded and the most recently
  published packages.
* **Settings** (`/settings`): the feed's policy in plain words (who may push and
  read, overwrite and delete behaviour, approval and promotion, mirror, license
  policy, retention) and the server version. It never shows keys or paths.

The gallery follows the feed's `read_api_key`. Turn it off, together with the
embedded documentation, with `enable_web_ui = false`; the NuGet endpoints keep
working.

## Turning on the admin area

The admin area exists only when an admin key is configured:

```toml
admin_api_key = "a-long-random-secret"
```

or `YANUGET_ADMIN_API_KEY`, or a feed's own `admin_api_key`. Open `/admin`
(`/{feed}/admin`); the browser asks for credentials: any user name, the admin
key as the password. A package's gallery page links to its admin page
("Manage versions").

## Version states

A version in a feed is in one or more of these states. The admin area shows
each one; clients see only what the last column says.

| State | How it gets there | What clients see |
| --- | --- | --- |
| **Listed** | Pushed, approved | In search and restore. |
| **Unlisted** | `dotnet nuget delete` while `hard_delete_enabled` is off | Hidden from search, still restorable by exact version. |
| **Disabled** | **Disable** in the admin area | Nothing: hidden everywhere and not downloadable (`404`) until re-enabled. |
| **Pending** | Entered a feed with `requires_approval` | Nothing, until an admin approves it. |
| **Flagged** | Failed a `license_policy` with `action = "warn"` | Same as listed; the flag is for the operator. |
| **Pinned** | **Pin** in the admin area | No difference; retention keeps it. |

Disable is the tool for a broken or compromised release: unlisting leaves it
downloadable for anyone who asks for that exact version, disabling does not.

## Everyday tasks

Everything below is done on a package's admin page, either with the buttons
next to one version or by ticking several versions and choosing an operation
for all of them.

* **Disable / enable** a version to withdraw it from clients and bring it back.
* **Approve** a pending version in a gated feed.
* **Promote** a version into the feed named by `promotes_to`, or **copy** or
  **move** versions to another feed. The page then says what happened in the
  target (added, pending there with a link to approve it, or already there),
  and each row shows whether the next feed holds the version (`in stable`,
  `pending in stable`). See [Feeds and release rings](feeds.md#release-rings).
* **Pin / unpin** a version to keep it from retention.
* **Delete** a version from this feed. When no other feed holds it, its
  package, symbols and attached files are removed from disk. A delete cannot be
  undone; the confirmation says when a version is pinned. In a mirror feed the
  deleted version is not fetched again.
* **Detach** an attached file from a version.

To remove a whole package, tick all its versions and delete them.

## Internalizer reports

A package that the Chocolatey internalizer rewrote carries a report at
`internalizer/report.json`. When a pending version has one, its admin page
shows it under the version list: the status and its reasons, the upstream
source and hash, the Chocolatey version and image the scripts ran in, each
original download URL with the attached file that replaces it and whether the
script's checksum vouched for it (*verified*) or not (*trust on first use*),
the warnings, and the prelude the install scripts run first.

Before you approve, the page checks the report against the version. A file the
report lists that is not attached, an attached file whose size or SHA-256
differs from the report, a prelude the package lacks, or a report the server
cannot read is listed in a warning, and **Approve** asks for confirmation.
That is what an interrupted run leaves behind, and approving it publishes an
install that fails. Run it again, or attach the missing files, first. The
selection's **Approve** does not check reports.

Everything in a report comes from the package and is shown as text; no URL in
it is a link. A report is read only for the ten newest pending versions, up to
1 MiB, with a prelude of up to 256 KiB. Its schema is version 1, defined in
the `yanuget-pkg` crate (`report` module); fields the server does not know are
ignored, and a report of another schema version is shown as unreadable. A
version without a report looks as it always did.

Every admin action can be scripted: with [`yanuget admin`](admin-cli.md), or as
a `POST` with a CSRF token (see
[Scripting promotion and approval](feeds.md#scripting-promotion-and-approval)
and [Admin area](api.md#admin-area-http-basic-auth)).

## Retention

Retention removes old versions automatically. It is off until you enable it and
set at least one limit:

```toml
[retention]
enabled = true
keep_latest_stable = 10        # per package
keep_latest_prerelease = 5
max_age_days = 180
interval_hours = 24            # 0: only when an admin starts it
```

The newest stable version of a package (or the newest pre-release, if it has no
stable one) is always kept, and pending, disabled and pinned versions are never
deleted by retention. A feed can override the rules in its own
`[feeds.retention]` table.

Before trusting the rules with your packages, open `/admin/retention`. It shows
the rules, what the last cleanup did, and every version the next cleanup would
delete, with the reason and the space it frees. Its button deletes exactly that
list and nothing else; if a push changed the plan in the meantime, nothing is
deleted and the page shows the new plan. All keys are in
[Configuration → Retention](configuration.md#retention).

## Protecting the admin area

* Use a long random admin key, different from the push key.
* The admin area uses HTTP Basic auth, so only reach it over HTTPS.
* Requests a browser marks as cross-site and requests without the CSRF token are
  refused, so a hostile page cannot drive a signed-in browser.
* If the admin area should not be reachable from everywhere, restrict
  `/admin` and `/{feed}/admin` at the reverse proxy.
