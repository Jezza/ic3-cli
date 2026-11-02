//! `ic3`: a Microsoft Teams chat command line for agents. See README.md.
#![allow(clippy::print_stdout, clippy::print_stderr)]

mod list;
mod output;
mod store;

use std::collections::HashSet;
use std::future::Future;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use clap::{Parser, Subcommand};
use ic3_teams::format::parse_slack;
use ic3_teams::logging::{RedactingMakeWriter, filter_directives};
use ic3_teams::{Client, ConversationId, Event, RichText, TokenStore};
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

use crate::output::{EventLine, ReceiptJson};
use crate::store::LockedStore;

/// Exit code for "not signed in, or the session expired".
const EXIT_SIGNED_OUT: u8 = 2;

/// Microsoft Teams chat for agents: send messages and stream events as JSON.
///
/// Unofficial: talks to Teams' undocumented chat service as the signed-in
/// account. Exit codes: 0 success, 1 failure, 2 not signed in (run `ic3 login`).
#[derive(Debug, Parser)]
#[command(name = "ic3", version)]
struct Cli {
    /// The session file [default: $XDG_DATA_HOME/ic3-cli/session.json]
    #[arg(long, global = true, env = "IC3_SESSION", value_name = "PATH")]
    session: Option<PathBuf>,
    /// The tenant to sign in to: `organizations`, a domain or a GUID
    #[arg(
        long,
        global = true,
        env = "IC3_TENANT",
        default_value = "organizations"
    )]
    tenant: String,
    /// More log output on stderr (-v info, -vv debug); RUST_LOG overrides
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Sign in with a device code (open the printed URL on any device)
    Login {
        /// Sign in again even if the stored session still works
        #[arg(long)]
        force: bool,
    },
    /// Forget the stored session
    Logout,
    /// List every chat and channel: `id<TAB>kind<TAB>name`, most recent first
    List {
        /// One JSON object per line instead
        #[arg(long)]
        json: bool,
    },
    /// Send a message to a conversation id (from `ic3 list`)
    Send {
        /// The conversation id
        id: String,
        /// The message; `-` reads it from stdin
        message: String,
        /// Send the text literally, without *bold* / _italic_ / `code` formatting
        #[arg(long)]
        plain: bool,
        /// Print the receipt as JSON
        #[arg(long)]
        json: bool,
    },
    /// Stream new, edited and deleted messages as JSON lines until killed
    Listen {
        /// Conversation ids to include (several per argument, comma-separated); all if none
        ids: Vec<String>,
    },
}

/// No usable session: exit code 2.
#[derive(Debug)]
struct SignedOut;

impl std::fmt::Display for SignedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not signed in, or the session has expired; run `ic3 login`")
    }
}

impl std::error::Error for SignedOut {}

fn is_signed_out(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.is::<SignedOut>()
            || matches!(
                c.downcast_ref::<ic3_teams::Error>(),
                Some(ic3_teams::Error::ReauthRequired)
            )
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if is_signed_out(&e) => {
            eprintln!("ic3: {SignedOut}");
            ExitCode::from(EXIT_SIGNED_OUT)
        }
        Err(e) => {
            eprintln!("ic3: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging(verbose: u8) {
    let base = std::env::var("RUST_LOG").unwrap_or_else(|_| {
        match verbose {
            0 => "warn",
            1 => "info",
            _ => "debug",
        }
        .to_owned()
    });
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter_directives(&base)))
        .with_writer(RedactingMakeWriter::new(std::io::stderr))
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}

fn default_session_path() -> anyhow::Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "ic3-cli")
        .ok_or_else(|| anyhow!("no home directory; pass --session"))?;
    Ok(dirs.data_dir().join("session.json"))
}

/// The session file and how to build clients on it.
#[derive(Debug)]
struct Session {
    store: Arc<LockedStore>,
    tenant: String,
}

impl Session {
    fn client(&self) -> anyhow::Result<Client> {
        let store: Arc<dyn TokenStore> = self.store.clone();
        Ok(Client::builder()
            .tenant(&self.tenant)
            .token_store_arc(store)
            .build()?)
    }

    /// A client on the stored session; [`SignedOut`] without one.
    async fn signed_in_client(&self) -> anyhow::Result<Client> {
        let client = self.client()?;
        if client.needs_login().await? {
            return Err(SignedOut.into());
        }
        Ok(client)
    }

    /// Runs `op` on a signed-in client. If the session is refused but
    /// another `ic3` process has saved a newer one meanwhile, runs it once
    /// more on a client built from that.
    async fn with_client<T, F, Fut>(&self, op: F) -> anyhow::Result<T>
    where
        F: Fn(Client) -> Fut,
        Fut: Future<Output = anyhow::Result<T>>,
    {
        let client = self.signed_in_client().await?;
        let first = op(client.clone()).await;
        client.shutdown().await;
        match first {
            Err(e) if is_signed_out(&e) && self.store.changed().await? => {
                tracing::info!("session replaced by another ic3 process; retrying with it");
                let client = self.signed_in_client().await?;
                let second = op(client.clone()).await;
                client.shutdown().await;
                second
            }
            r => r,
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let path = match cli.session {
        Some(p) => p,
        None => default_session_path()?,
    };
    let session = Session {
        store: Arc::new(LockedStore::new(path)),
        tenant: cli.tenant,
    };
    match cli.command {
        Cmd::Login { force } => login(&session, force).await,
        Cmd::Logout => {
            session.store.force_clear().await?;
            eprintln!("signed out ({})", session.store.path().display());
            Ok(())
        }
        Cmd::List { json } => {
            let rows = session
                .with_client(|c| async move { list::fetch(&c).await })
                .await?;
            let mut out = std::io::stdout().lock();
            for row in rows {
                let line = if json {
                    serde_json::to_string(&row)?
                } else {
                    row.tsv()
                };
                writeln!(out, "{line}")?;
            }
            out.flush()?;
            Ok(())
        }
        Cmd::Send {
            id,
            message,
            plain,
            json,
        } => send(&session, id, message, plain, json).await,
        Cmd::Listen { ids } => listen(&session, &ids).await,
    }
}

async fn login(session: &Session, force: bool) -> anyhow::Result<()> {
    let client = session.client()?;
    if !force && !client.needs_login().await? {
        match client.me().await {
            Ok(me) => {
                eprintln!("already signed in as {}", me.display_name);
                client.shutdown().await;
                return Ok(());
            }
            Err(ic3_teams::Error::ReauthRequired) => {
                // Expired: sign in afresh below, on a new client.
            }
            Err(e) => return Err(e).context("checking the stored session"),
        }
    }
    client.shutdown().await;
    let client = session.client()?;
    let login = client
        .start_device_login()
        .await
        .context("starting the sign-in")?;
    let prompt = login.prompt();
    eprintln!("{}", prompt.message);
    eprintln!();
    eprintln!("  URL:  {}", prompt.verification_url);
    eprintln!("  Code: {}", prompt.user_code);
    login.wait().await.context("signing in")?;
    let me = client.me().await?;
    eprintln!("signed in as {}", me.display_name);
    client.shutdown().await;
    Ok(())
}

/// The message text: the argument, or stdin for `-`; trailing line breaks
/// dropped; empty is an error.
fn message_text(arg: String) -> anyhow::Result<String> {
    let text = if arg == "-" {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("reading the message from stdin")?;
        s
    } else {
        arg
    };
    let text = text.trim_end_matches(['\n', '\r']).to_owned();
    if text.trim().is_empty() {
        bail!("the message is empty");
    }
    Ok(text)
}

fn body(text: &str, plain: bool) -> RichText {
    if plain {
        RichText::from_plain_text(text)
    } else {
        parse_slack(text)
    }
}

async fn send(
    session: &Session,
    id: String,
    message: String,
    plain: bool,
    json: bool,
) -> anyhow::Result<()> {
    let id = id.trim().to_owned();
    if id.is_empty() {
        bail!("the conversation id is empty");
    }
    let conv = ConversationId(id);
    let body = body(&message_text(message)?, plain);
    let receipt = session
        .with_client(|c| {
            let (conv, body) = (conv.clone(), body.clone());
            async move { Ok(c.api().send(&conv, &body, None).await?) }
        })
        .await
        .with_context(|| format!("sending to {}", conv.0))?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&ReceiptJson::new(&conv, &receipt))?
        );
    }
    Ok(())
}

/// Every id in `args`, each of which may hold several separated by commas.
fn parse_ids(args: &[String]) -> HashSet<String> {
    args.iter()
        .flat_map(|a| a.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// How one push connection ended.
enum Ended {
    /// Killed: exit cleanly.
    Signal,
    /// The session was refused.
    SignedOut,
    /// stdout was closed: nobody is reading.
    StdoutClosed,
}

async fn listen(session: &Session, ids: &[String]) -> anyhow::Result<()> {
    let filter = parse_ids(ids);
    let mut sigterm = signal(SignalKind::terminate()).context("installing the SIGTERM handler")?;
    loop {
        let client = session.signed_in_client().await?;
        let ended = match listen_once(&client, &filter, &mut sigterm).await {
            Ok(ended) => ended,
            Err(e) if is_signed_out(&e) => Ended::SignedOut,
            Err(e) => {
                client.shutdown().await;
                return Err(e);
            }
        };
        client.shutdown().await;
        match ended {
            Ended::Signal | Ended::StdoutClosed => return Ok(()),
            Ended::SignedOut if session.store.changed().await? => {
                tracing::info!("session replaced by another ic3 process; reconnecting with it");
            }
            Ended::SignedOut => return Err(SignedOut.into()),
        }
    }
}

async fn listen_once(
    client: &Client,
    filter: &HashSet<String>,
    sigterm: &mut tokio::signal::unix::Signal,
) -> anyhow::Result<Ended> {
    let me = client.me().await?.mri;
    let mut events = client.connect().await.context("connecting to Teams")?;
    loop {
        let ev = tokio::select! {
            ev = events.recv() => ev,
            _ = tokio::signal::ctrl_c() => return Ok(Ended::Signal),
            _ = sigterm.recv() => return Ok(Ended::Signal),
        };
        let Some(ev) = ev else {
            bail!("the push connection stopped");
        };
        if let Some(line) = EventLine::from_event(&ev, &me) {
            if !filter.is_empty() && !filter.contains(line.conversation()) {
                continue;
            }
            if !print_line(&line)? {
                return Ok(Ended::StdoutClosed);
            }
            continue;
        }
        match ev {
            Event::Connected => tracing::info!("connected"),
            Event::Disconnected { reason, retry_in } => {
                tracing::warn!(?reason, ?retry_in, "push connection dropped; reconnecting");
            }
            Event::NeedsCatchUp { reason } => {
                tracing::warn!(
                    ?reason,
                    "messages may have been missed (ic3 does not catch up)"
                );
            }
            Event::ReauthRequired => return Ok(Ended::SignedOut),
            _ => {}
        }
    }
}

/// Writes one JSON line and flushes; `false` when stdout is closed.
fn print_line(line: &EventLine) -> anyhow::Result<bool> {
    let json = serde_json::to_string(line)?;
    let mut out = std::io::stdout().lock();
    match writeln!(out, "{json}").and_then(|()| out.flush()) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(false),
        Err(e) => Err(e).context("writing to stdout"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_split_on_commas_and_ignore_blanks() {
        let args = vec!["a,b".to_owned(), " c ".to_owned(), ",,".to_owned()];
        let got = parse_ids(&args);
        assert_eq!(got, HashSet::from(["a".into(), "b".into(), "c".into()]));
    }

    #[test]
    fn message_text_drops_trailing_newlines_only() {
        assert_eq!(message_text("hi there\n\n".into()).unwrap(), "hi there");
        assert_eq!(message_text("  indented".into()).unwrap(), "  indented");
    }

    #[test]
    fn empty_message_is_an_error() {
        assert!(message_text(" \n".into()).is_err());
    }

    #[test]
    fn signed_out_errors_map_to_exit_two() {
        assert!(is_signed_out(&anyhow::Error::from(SignedOut)));
        let e = anyhow::Error::from(ic3_teams::Error::ReauthRequired).context("sending to x");
        assert!(is_signed_out(&e));
        let other = anyhow::Error::from(ic3_teams::Error::Network("down".into()));
        assert!(!is_signed_out(&other));
    }

    #[test]
    fn plain_keeps_markup_literal() {
        assert_eq!(body("*hi*", true).plain_text(), "*hi*");
        assert_eq!(body("*hi*", false).plain_text(), "hi");
    }

    #[test]
    fn cli_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from(["ic3", "send", "48:notes", "-", "--plain"]).unwrap();
        assert!(matches!(cli.command, Cmd::Send { plain: true, .. }));
    }
}
