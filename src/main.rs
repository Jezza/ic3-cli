//! `ic3`: a Microsoft Teams chat command line for agents. See README.md.
#![allow(clippy::print_stdout, clippy::print_stderr)]

mod list;
mod output;
mod store;
mod typing;

use std::collections::HashSet;
use std::future::Future;
use std::io::{IsTerminal, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use clap::{Parser, Subcommand};
use ic3_teams::format::parse_slack;
use ic3_teams::logging::{RedactingMakeWriter, filter_directives};
use ic3_teams::redact::redact_text;
use ic3_teams::{Client, ConversationId, Event, RichText, TokenStore};
use tokio::io::{AsyncWriteExt, Interest};
use tokio::net::unix::pipe;
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

use crate::output::{EventLine, ReceiptJson};
use crate::store::{LockedStore, Status};

/// Exit code for "not signed in, or the session expired".
const EXIT_SIGNED_OUT: u8 = 2;
/// How often `listen` checks whether someone signed in again or out.
const STATUS_EVERY: Duration = Duration::from_secs(30);

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
    /// Forget the stored session (running `ic3 listen`s stop with exit code 2)
    Logout,
    /// List every chat and channel: `id<TAB>kind<TAB>name`, most recent first
    List {
        /// One JSON object per line instead
        #[arg(long)]
        json: bool,
    },
    /// Send a message to a conversation id (from `ic3 list`); stops `ic3 typing` there first
    Send {
        /// The conversation id
        id: String,
        /// The message; `-` reads it from stdin (which must be a pipe or file)
        message: String,
        /// Send the text literally, without *bold* / _italic_ / `code` formatting
        #[arg(long)]
        plain: bool,
        /// Print the receipt as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show "typing…" in a conversation until you post there
    ///
    /// How Teams typing works: there is no typing state to switch on and off. A client
    /// shows it is typing by sending a signal into the chat, which the others see as
    /// "… is typing" for about 25 seconds. To keep the indicator on, it repeats the
    /// signal every 20 seconds. Nothing is sent to stop it: the indicator ends when the
    /// signals stop and it expires, or as soon as the sender's next message arrives.
    ///
    /// `ic3 typing <id>` sends the signal at once and then every 20 seconds. Run it in
    /// the background when an agent's turn starts, and post when the turn ends:
    ///
    ///     ic3 typing '<id>' &
    ///     ...the agent works...
    ///     ic3 send '<id>' 'the reply'
    ///
    /// `ic3 send` to the same conversation stops it before posting, so no typing signal
    /// follows the message, and the message itself clears the indicator for everyone.
    /// It also stops on SIGINT or SIGTERM, when another `ic3 typing` starts for the same
    /// conversation, or after --for, so a crashed agent can't leave people watching
    /// "typing…" for ever.
    ///
    /// Prints nothing. Exits 0 when stopped, 2 if not signed in.
    #[command(verbatim_doc_comment)]
    Typing {
        /// The conversation id
        id: String,
        /// Stop after this long at most: 90s, 5m, 1h
        #[arg(long = "for", value_name = "DURATION", default_value = "5m", value_parser = typing::parse_duration)]
        max: Duration,
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
            // Error text can quote response bodies: redact it like the log.
            eprintln!("ic3: {}", redact_text(&format!("{e:#}")));
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
    path: PathBuf,
    store: Arc<LockedStore>,
    tenant: String,
}

impl Session {
    fn client_on(&self, store: Arc<LockedStore>) -> anyhow::Result<Client> {
        let store: Arc<dyn TokenStore> = store;
        Ok(Client::builder()
            .tenant(&self.tenant)
            .token_store_arc(store)
            .build()?)
    }

    /// A fresh client on the stored session; [`SignedOut`] without one.
    async fn signed_in_client(&self) -> anyhow::Result<Client> {
        self.store.reset().await;
        let client = self.client_on(self.store.clone())?;
        if client.needs_login().await? {
            return Err(SignedOut.into());
        }
        Ok(client)
    }

    /// Runs `op` on a signed-in client. If the session is refused but
    /// another `ic3` process has saved a newer one meanwhile, runs it once
    /// more on a client built from that. A refused session fails before
    /// anything is posted, so the retry cannot double-send.
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
        store: Arc::new(LockedStore::new(&path)),
        path,
        tenant: cli.tenant,
    };
    match cli.command {
        Cmd::Login { force } => login(&session, force).await,
        Cmd::Logout => {
            session.store.sign_out().await?;
            eprintln!("signed out ({})", session.path.display());
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
        Cmd::Typing { id, max } => typing(&session, id, max).await,
    }
}

async fn login(session: &Session, force: bool) -> anyhow::Result<()> {
    if !force {
        let client = session.client_on(session.store.clone())?;
        let signed_in = if client.needs_login().await? {
            None
        } else {
            match client.me().await {
                Ok(me) => Some(me),
                // Expired: sign in afresh below.
                Err(ic3_teams::Error::ReauthRequired) => None,
                Err(e) => return Err(e).context("checking the stored session"),
            }
        };
        client.shutdown().await;
        if let Some(me) = signed_in {
            eprintln!("already signed in as {}", me.display_name);
            return Ok(());
        }
    }
    // The device flow's save is a new sign-in: running `ic3` processes on
    // the old one stop saving it and reconnect with this one.
    let client = session.client_on(Arc::new(LockedStore::for_sign_in(&session.path)))?;
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
        let mut stdin = std::io::stdin();
        if stdin.is_terminal() {
            bail!("`-` reads the message from stdin, but stdin is a terminal; pipe the message in");
        }
        let mut s = String::new();
        stdin
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
    stop_typing(session, &conv).await;
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

/// Stops an `ic3 typing` in `conv`, waiting out a signal in flight so none
/// follows the post. A failure only costs a stray indicator: warn and go on.
async fn stop_typing(session: &Session, conv: &ConversationId) {
    let dir = typing::dir_for(&session.path);
    let id = conv.0.clone();
    match tokio::task::spawn_blocking(move || typing::stop(&dir, &id)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "cannot stop ic3 typing"),
        Err(e) => tracing::warn!(error = %e, "cannot stop ic3 typing"),
    }
}

async fn typing(session: &Session, id: String, max: Duration) -> anyhow::Result<()> {
    let id = id.trim().to_owned();
    if id.is_empty() {
        bail!("the conversation id is empty");
    }
    let conv = ConversationId(id);
    let mut sigint = signal(SignalKind::interrupt()).context("installing the SIGINT handler")?;
    let mut sigterm = signal(SignalKind::terminate()).context("installing the SIGTERM handler")?;
    let marker = typing::Marker::claim(&typing::dir_for(&session.path), &conv.0)
        .context("claiming the conversation for typing")?;
    let result = tokio::select! {
        r = typing_loop(session, &conv, &marker) => r,
        _ = sigint.recv() => Ok(()),
        _ = sigterm.recv() => Ok(()),
        () = tokio::time::sleep(max) => Ok(()),
    };
    if let Err(e) = marker.release() {
        tracing::warn!(error = %e, "cannot remove the typing marker");
    }
    result
}

/// How soon a failed typing signal is retried.
const TYPING_RETRY: Duration = Duration::from_secs(5);
/// How often the typer checks whether `send` or a newer typer stopped it.
const TYPING_POLL: Duration = Duration::from_secs(1);

async fn typing_loop(
    session: &Session,
    conv: &ConversationId,
    marker: &typing::Marker,
) -> anyhow::Result<()> {
    let mut client = session.signed_in_client().await?;
    let mut poll = tokio::time::interval(TYPING_POLL);
    let mut next = tokio::time::Instant::now();
    loop {
        poll.tick().await;
        // Held until the signal is sent, so `send` can't post in between.
        let Some(guard) = marker.hold_if_current()? else {
            client.shutdown().await;
            return Ok(());
        };
        if tokio::time::Instant::now() < next {
            continue;
        }
        match client.api().send_typing(conv).await {
            Ok(()) => next = tokio::time::Instant::now() + ic3_teams::TYPING_REFRESH,
            Err(ic3_teams::Error::ReauthRequired) => {
                drop(guard);
                client.shutdown().await;
                if !session.store.changed().await? {
                    return Err(SignedOut.into());
                }
                tracing::info!("session replaced by another ic3 process; carrying on with it");
                client = session.signed_in_client().await?;
            }
            Err(e) => {
                tracing::warn!(error = %e, "typing signal failed; retrying");
                next = tokio::time::Instant::now() + TYPING_RETRY;
            }
        }
    }
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

/// Why one push connection ended.
#[derive(Debug, PartialEq)]
enum Ended {
    /// The session was refused, or signed out.
    SignedOut,
    /// Someone signed in again: reconnect with the new session.
    Replaced,
    /// stdout was closed: nobody is reading.
    StdoutClosed,
}

/// `listen`'s stdout. A pipe (the usual case) is written without blocking a
/// thread, so a signal can always interrupt a write to a reader that stopped
/// reading, and is watched for its reader closing.
#[derive(Debug)]
enum Stdout {
    Pipe(pipe::Sender),
    /// A file or terminal.
    Other(tokio::sync::Mutex<tokio::io::Stdout>),
}

impl Stdout {
    fn new() -> Stdout {
        match std::io::stdout().as_fd().try_clone_to_owned() {
            Ok(fd) => Stdout::from_fd(fd),
            Err(_) => Stdout::Other(tokio::sync::Mutex::new(tokio::io::stdout())),
        }
    }

    /// A pipe sender when `fd` is a pipe, else plain stdout.
    fn from_fd(fd: OwnedFd) -> Stdout {
        match pipe::Sender::from_owned_fd(fd) {
            Ok(s) => Stdout::Pipe(s),
            Err(_) => Stdout::Other(tokio::sync::Mutex::new(tokio::io::stdout())),
        }
    }

    async fn write_all(&self, mut buf: &[u8]) -> std::io::Result<()> {
        match self {
            Stdout::Pipe(s) => {
                while !buf.is_empty() {
                    s.writable().await?;
                    match s.try_write(buf) {
                        Ok(n) => buf = &buf[n..],
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => return Err(e),
                    }
                }
                Ok(())
            }
            Stdout::Other(out) => {
                let mut out = out.lock().await;
                out.write_all(buf).await?;
                out.flush().await
            }
        }
    }

    /// Resolves once the reader of a pipe has gone; never for a file or
    /// terminal.
    async fn closed(&self) {
        if let Stdout::Pipe(s) = self
            && s.ready(Interest::ERROR).await.is_ok()
        {
            return;
        }
        std::future::pending::<()>().await;
    }
}

async fn listen(session: &Session, ids: &[String]) -> anyhow::Result<()> {
    let filter = parse_ids(ids);
    // Installed once, up front, so no signal is lost between connections.
    let mut sigint = signal(SignalKind::interrupt()).context("installing the SIGINT handler")?;
    let mut sigterm = signal(SignalKind::terminate()).context("installing the SIGTERM handler")?;
    let out = Stdout::new();
    let result = tokio::select! {
        r = listen_loop(session, &filter, &out) => r,
        _ = sigint.recv() => Ok(()),
        _ = sigterm.recv() => Ok(()),
        () = out.closed() => Ok(()),
    };
    match result {
        // Leave without waiting on the runtime: a write to a terminal or file
        // may still be in flight on a blocking thread.
        Ok(()) => std::process::exit(0),
        Err(e) => Err(e),
    }
}

async fn listen_loop(
    session: &Session,
    filter: &HashSet<String>,
    out: &Stdout,
) -> anyhow::Result<()> {
    loop {
        let client = session.signed_in_client().await?;
        let ended = match listen_once(session, &client, filter, out).await {
            Ok(ended) => ended,
            Err(e) if is_signed_out(&e) => Ended::SignedOut,
            Err(e) => {
                client.shutdown().await;
                return Err(e);
            }
        };
        client.shutdown().await;
        match ended {
            Ended::StdoutClosed => return Ok(()),
            Ended::Replaced => tracing::info!("signed in again; reconnecting with the new session"),
            Ended::SignedOut if session.store.changed().await? => {
                tracing::info!("session replaced by another ic3 process; reconnecting with it");
            }
            Ended::SignedOut => return Err(SignedOut.into()),
        }
    }
}

async fn listen_once(
    session: &Session,
    client: &Client,
    filter: &HashSet<String>,
    out: &Stdout,
) -> anyhow::Result<Ended> {
    let me = client.me().await?.mri;
    let mut events = client.connect().await.context("connecting to Teams")?;
    let mut check = tokio::time::interval(STATUS_EVERY);
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let ev = tokio::select! {
            ev = events.recv() => ev,
            _ = check.tick() => match session.store.status().await? {
                Status::Current => continue,
                Status::Replaced => return Ok(Ended::Replaced),
                Status::SignedOut => return Ok(Ended::SignedOut),
            },
        };
        let Some(ev) = ev else {
            bail!("the push connection stopped");
        };
        if let Some(line) = EventLine::from_event(&ev, &me) {
            if !filter.is_empty() && !filter.contains(line.conversation()) {
                continue;
            }
            if !write_line(out, &line).await? {
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

/// Writes one JSON line; `false` when the reader has gone.
async fn write_line(out: &Stdout, line: &EventLine) -> anyhow::Result<bool> {
    let mut json = serde_json::to_string(line)?;
    json.push('\n');
    match out.write_all(json.as_bytes()).await {
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
    fn typing_parses_its_limit() {
        let cli = Cli::try_parse_from(["ic3", "typing", "48:notes", "--for", "90s"]).unwrap();
        assert!(matches!(cli.command, Cmd::Typing { max, .. } if max == Duration::from_secs(90)));
        let cli = Cli::try_parse_from(["ic3", "typing", "48:notes"]).unwrap();
        assert!(matches!(cli.command, Cmd::Typing { max, .. } if max == Duration::from_secs(300)));
        assert!(Cli::try_parse_from(["ic3", "typing", "48:notes", "--for", "soon"]).is_err());
    }

    #[test]
    fn cli_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from(["ic3", "send", "48:notes", "-", "--plain"]).unwrap();
        assert!(matches!(cli.command, Cmd::Send { plain: true, .. }));
    }

    fn delete_line() -> EventLine {
        EventLine::Delete {
            conversation: "c".into(),
            id: "1".into(),
        }
    }

    #[tokio::test]
    async fn pipe_stdout_notices_the_reader_going() {
        let (reader, writer) = std::io::pipe().unwrap();
        let out = Arc::new(Stdout::from_fd(OwnedFd::from(writer)));
        assert!(matches!(*out, Stdout::Pipe(_)));
        let watch = tokio::spawn({
            let out = out.clone();
            async move { out.closed().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!watch.is_finished(), "resolved while the reader was open");
        drop(reader);
        tokio::time::timeout(Duration::from_secs(2), watch)
            .await
            .expect("did not notice the reader closing")
            .unwrap();
        assert!(!write_line(&out, &delete_line()).await.unwrap());
    }

    #[tokio::test]
    async fn pipe_stdout_writes_lines() {
        let (mut reader, writer) = std::io::pipe().unwrap();
        let out = Stdout::from_fd(OwnedFd::from(writer));
        assert!(write_line(&out, &delete_line()).await.unwrap());
        drop(out);
        let mut got = String::new();
        reader.read_to_string(&mut got).unwrap();
        assert_eq!(
            got,
            "{\"type\":\"delete\",\"conversation\":\"c\",\"id\":\"1\"}\n"
        );
    }

    #[tokio::test]
    async fn a_stalled_reader_does_not_block_cancellation() {
        let (_reader, writer) = std::io::pipe().unwrap();
        let out = Stdout::from_fd(OwnedFd::from(writer));
        let big = vec![b'x'; 1 << 20];
        let r = tokio::time::timeout(Duration::from_millis(200), out.write_all(&big)).await;
        assert!(
            r.is_err(),
            "a write to a full pipe should wait, and be cancellable"
        );
    }

    #[tokio::test]
    async fn a_regular_file_is_not_a_pipe() {
        let file = tempfile::tempfile().unwrap();
        let out = Stdout::from_fd(OwnedFd::from(file));
        assert!(matches!(out, Stdout::Other(_)));
        let r = tokio::time::timeout(Duration::from_millis(100), out.closed()).await;
        assert!(r.is_err());
    }

    #[test]
    fn error_lines_are_redacted() {
        let e = anyhow!("http 400: {{\"refresh_token\":\"0.AXoA-secret\"}}");
        let printed = redact_text(&format!("{e:#}"));
        assert!(!printed.contains("0.AXoA-secret"), "{printed}");
    }
}
