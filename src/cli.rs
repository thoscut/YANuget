//! The `admin` and `config` sub-commands: thin argument handling over
//! [`yanuget::admin_client`] and [`yanuget::config_check`].

use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::{Args, Subcommand};

use yanuget::admin_client::{Action, AdminClient, CertPolicy};
use yanuget::config::{Config, DEFAULT_FEED};

/// Arguments for `yanuget admin`.
#[derive(Debug, Args)]
pub struct AdminArgs {
    /// Server to talk to, e.g. https://nuget.example.com (the address
    /// clients use, without the feed). Without it, the server on this
    /// machine, at the port and scheme of the configuration.
    #[arg(long, env = "YANUGET_ADMIN_URL")]
    url: Option<String>,
    /// Feed to act on. Without it, the only feed (or the single implicit
    /// one); a configuration with several feeds needs it.
    #[arg(long)]
    feed: Option<String>,
    /// Read the admin key from this file. Otherwise it comes from
    /// YANUGET_ADMIN_KEY, and failing that from the feed's admin key in the
    /// configuration. There is deliberately no flag for the key itself: a
    /// value on the command line is visible in `ps`.
    #[arg(long)]
    admin_key_file: Option<PathBuf>,
    /// PEM file of extra CA certificates to trust for --url (an internal CA).
    #[arg(long)]
    ca_cert: Option<PathBuf>,
    #[command(subcommand)]
    action: AdminAction,
}

/// What `yanuget admin` does.
#[derive(Debug, Subcommand)]
enum AdminAction {
    /// Copy a version into the feed's `promotes_to` feed.
    Promote(VersionArgs),
    /// Release a version that is pending in this feed.
    Approve(VersionArgs),
    /// Make a disabled version downloadable again.
    Enable(VersionArgs),
    /// Hide a version from clients without deleting it.
    Disable(VersionArgs),
    /// Keep a version from retention.
    Pin(VersionArgs),
    /// Let retention consider a version again.
    Unpin(VersionArgs),
    /// Remove a version from this feed for good.
    Delete(DeleteArgs),
}

#[derive(Debug, Args)]
struct VersionArgs {
    /// Package id.
    id: String,
    /// Version.
    version: String,
}

#[derive(Debug, Args)]
struct DeleteArgs {
    #[command(flatten)]
    target: VersionArgs,
    /// Confirm the delete; it cannot be undone.
    #[arg(long)]
    yes: bool,
}

pub async fn run_admin(config_path: Option<&str>, args: AdminArgs) -> anyhow::Result<()> {
    let (action, target) = match &args.action {
        AdminAction::Promote(t) => (Action::Promote, t),
        AdminAction::Approve(t) => (Action::Approve, t),
        AdminAction::Enable(t) => (Action::Enable, t),
        AdminAction::Disable(t) => (Action::Disable, t),
        AdminAction::Pin(t) => (Action::Pin, t),
        AdminAction::Unpin(t) => (Action::Unpin, t),
        AdminAction::Delete(d) => {
            if !d.yes {
                bail!("delete cannot be undone; add --yes to delete it");
            }
            (Action::Delete, &d.target)
        }
    };

    let key_given = admin_key_override(&args)?;
    // The configuration gives the local server's address, the feed's prefix
    // and its admin key. A remote --url needs it only when one is named.
    let config = if args.url.is_none() || config_path.is_some() {
        Some(Config::load(config_path).context("cannot load the configuration")?)
    } else {
        None
    };

    let (prefix, config_key) = match &config {
        Some(config) => feed_from_config(config, args.feed.as_deref())?,
        None => (
            args.feed
                .as_deref()
                .map_or(String::new(), |f| format!("/{f}")),
            None,
        ),
    };
    let key = key_given.or(config_key).context(
        "no admin key: set YANUGET_ADMIN_KEY, pass --admin-key-file, or configure the feed's \
         admin_api_key",
    )?;

    let (base, certs) = match (&args.url, &config) {
        (Some(url), _) => {
            let certs = match &args.ca_cert {
                Some(path) => CertPolicy::ExtraCa(
                    std::fs::read(path).with_context(|| format!("--ca-cert {}", path.display()))?,
                ),
                None => CertPolicy::System,
            };
            (url.trim_end_matches('/').to_string(), certs)
        }
        (None, Some(config)) => this_machine(config),
        (None, None) => unreachable!("loaded above when no --url is given"),
    };

    let client = AdminClient::new(&format!("{base}{prefix}"), &key, certs)?;
    let line = client.run(action, &target.id, &target.version).await?;
    println!("{line}");
    Ok(())
}

/// The server on this machine. A server with `allowed_hosts` (or a base URL)
/// refuses a request for `127.0.0.1`, so it is asked for by the name it
/// serves, resolved to its own address.
fn this_machine(config: &Config) -> (String, CertPolicy) {
    let addr = crate::local_addr(config);
    let name = config
        .base_url
        .as_deref()
        .and_then(yanuget::tls::host_of)
        .into_iter()
        .chain(config.host_allowlist().unwrap_or_default())
        .find(|h| h.parse::<std::net::IpAddr>().is_err() && !h.starts_with("*."));
    match name {
        Some(name) => (
            format!("{}://{name}:{}", config.scheme(), addr.port()),
            CertPolicy::ThisMachine {
                pin: Some((name, addr)),
            },
        ),
        None => (
            crate::local_base_url(config),
            CertPolicy::ThisMachine { pin: None },
        ),
    }
}

/// The key from --admin-key-file or YANUGET_ADMIN_KEY, if either is given.
fn admin_key_override(args: &AdminArgs) -> anyhow::Result<Option<String>> {
    let from_env = std::env::var("YANUGET_ADMIN_KEY").ok();
    match (&args.admin_key_file, from_env) {
        (Some(_), Some(_)) => {
            bail!("--admin-key-file and YANUGET_ADMIN_KEY are both given; use one")
        }
        (Some(path), None) => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("--admin-key-file {}", path.display()))?;
            Ok(Some(raw.trim_end_matches(['\r', '\n']).to_string()))
        }
        (None, env) => Ok(env),
    }
}

/// The feed's URL prefix on the server and its admin key, as the
/// configuration defines them.
fn feed_from_config(
    config: &Config,
    feed: Option<&str>,
) -> anyhow::Result<(String, Option<String>)> {
    let feeds = config.resolved_feeds()?;
    let chosen = match feed {
        Some(name) => feeds
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case(name))
            .with_context(|| {
                format!(
                    "no feed {name:?} in the configuration (feeds: {})",
                    names(&feeds)
                )
            })?,
        None if feeds.len() == 1 => &feeds[0],
        None => bail!(
            "the configuration has several feeds; pick one with --feed ({})",
            names(&feeds)
        ),
    };
    Ok((chosen.prefix.clone(), chosen.admin_api_key.clone()))
}

fn names(feeds: &[yanuget::config::ResolvedFeed]) -> String {
    let names: Vec<&str> = feeds.iter().map(|f| f.name.as_str()).collect();
    if names == [DEFAULT_FEED] {
        DEFAULT_FEED.to_string()
    } else {
        names.join(", ")
    }
}

/// Arguments for `yanuget config`.
#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Load the configuration as the server would (the file, the environment
    /// and key files) and check what it points at: the TLS certificate and
    /// key belong together and name the configured host, mirror CA bundles
    /// parse, promotion targets exist. Exits 1 on any error.
    Check,
}

pub fn run_config(config_path: Option<&str>, args: ConfigArgs) -> anyhow::Result<()> {
    match args.command {
        ConfigCommand::Check => {
            let report = yanuget::config_check::check(config_path);
            for e in &report.errors {
                println!("error: {e}");
            }
            for w in &report.warnings {
                println!("warning: {w}");
            }
            for n in &report.notes {
                println!("note: {n}");
            }
            if !report.is_ok() {
                bail!(
                    "the configuration has {} error{}",
                    report.errors.len(),
                    if report.errors.len() == 1 { "" } else { "s" }
                );
            }
            println!(
                "configuration ok{}",
                config_path.map_or(String::new(), |p| format!(": {p}"))
            );
            Ok(())
        }
    }
}
