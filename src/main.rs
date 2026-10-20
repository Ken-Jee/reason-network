mod agent;
mod claude;
mod client;
mod config;
mod daemon;
mod doing;
mod files;
mod git;
mod hook;
mod invite;
mod ipc;
mod mcp;
mod migrate;
mod net;
mod paths;
mod proto;
mod render;
mod setup;
mod store;
mod team;
mod tray;
mod util;
mod wizard;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use client::Link;
use proto::*;
use std::{io::Read, time::Duration};

/// Reason Network's page; team links point at its join page.
pub const SITE_URL: &str = "https://reasonmachines.com/network";

/// The name agents know this by: their MCP server, plugin and extension files, hook names.
pub const AGENT_NAME: &str = "reason-network";
/// The name this engine used before it was Reason Network: `setup` takes out what this binary wired in under it, and
/// `paths` moves the state it kept under it.
pub const LEGACY_NAME: &str = "tincan";

/// Where releases are published: a standalone install and its updates come from here.
pub const REPO: &str = "reason-machines/reason-network";

/// Whether the Reason CLI installed this binary (in `~/.reason/bin`): then it sets up and updates with the CLI, and
/// people type `reason network`.
pub fn managed() -> bool {
    static MANAGED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *MANAGED.get_or_init(|| {
        let real = |p: std::path::PathBuf| p.canonicalize().unwrap_or(p);
        util::current_exe().is_ok_and(|p| real(p).starts_with(real(paths::user_home().join(".reason").join("bin"))))
    })
}

/// The command people type, for messages (`{CLI}`): `reason network` with the Reason CLI, `reason-network` on its own.
pub struct Command;
pub const CLI: Command = Command;
impl std::fmt::Display for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(if managed() { "reason network" } else { AGENT_NAME })
    }
}

/// The engine as a command agents can run here: its full path where the Reason CLI manages it (whose `reason network`
/// doesn't take every engine command and flag), `reason-network` on its own.
pub struct Engine;
pub const ENGINE: Engine = Engine;
impl std::fmt::Display for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match setup::exe() {
            Ok(p) if managed() => f.write_str(setup::shell_cmd(&p, "").trim_end()),
            _ => f.write_str(AGENT_NAME),
        }
    }
}

/// The one line that installs Reason Network on its own (no account needed) and runs `reason-network <args>`: for
/// macOS and Linux, and for Windows.
pub fn install_lines(args: &str) -> [String; 2] {
    let base = format!("https://github.com/{REPO}/releases/latest/download");
    [
        format!("curl -fsSL {base}/install.sh | sh -s -- {args}"),
        format!("powershell -c \"& ([scriptblock]::Create((irm {base}/install.ps1))) {args}\""),
    ]
}

/// The install script, run without questions, installing into `dir`.
pub fn installer(dir: &std::path::Path) -> std::process::Command {
    let base = format!("https://github.com/{REPO}/releases/latest/download");
    let mut cmd;
    if cfg!(windows) {
        cmd = std::process::Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", &format!("irm {base}/install.ps1 | iex")]);
    } else {
        cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(format!("s=\"$(curl -fsSL {base}/install.sh)\" && printf '%s\\n' \"$s\" | sh"));
    }
    cmd.env("REASON_NETWORK_BIN_DIR", dir).env("REASON_NETWORK_NO_PROMPT", "1");
    cmd
}

/// The team link: Reason Network's join page, carrying the `ticket`.
pub fn join_url(ticket: &str) -> String {
    format!("{SITE_URL}/join/#{ticket}")
}

#[derive(Parser)]
#[command(
    name = "reason-network",
    version,
    about = "Reason Network: your agents, talking to your teammates' agents",
    after_help = "Run it on its own to get set up, then to see who's working on what."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show who is working on what, across the team
    Who {
        /// Only sessions matching this text
        filter: Option<String>,
        /// Include sessions that went offline in the last day, and offline machines
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Send a message to an agent session (`person/session`), all of a person's (`person/*`), `repo:<name>` or `*`.
    /// It comes from the agent session whose shell runs it, `--from <session>`, or from a plain terminal,
    /// `<you>/terminal` (read replies with `listen --as terminal`)
    Send {
        to: String,
        /// The message; read from stdin when omitted or `-`
        message: Vec<String>,
        #[arg(long)]
        reply_to: Option<String>,
        /// Don't wake the recipient; it reads this at its next natural pause
        #[arg(long)]
        fyi: bool,
        /// Also pop it up on the person's screen (then `<to>` may be your own name)
        #[arg(long, conflicts_with = "fyi")]
        notify: bool,
        /// Send as this agent session here (e.g. one made with `listen --as <name>`)
        #[arg(long)]
        from: Option<String>,
        /// A text file to send with it (repeat for more): at most 10, 256 KB each, 512 KB in all
        #[arg(long = "file", value_name = "PATH")]
        files: Vec<String>,
    },
    /// Set what the current agent session is doing (run from inside the agent's shell)
    Status {
        doing: Vec<String>,
        #[arg(long)]
        name: Option<String>,
    },
    /// Start a team, with you as its first member
    Init {
        team: Option<String>,
        /// Your name in the team (default: $USER)
        #[arg(long = "as")]
        person: Option<String>,
        /// Don't configure agents or install the background service
        #[arg(long)]
        no_setup: bool,
        /// Your own iroh relay, from the start (see `relay`)
        #[arg(long)]
        relay: Option<String>,
    },
    /// Show the team link to send friends (or, with --me / --code, a one-time code)
    Invite {
        /// A one-time code for another machine of yours
        #[arg(long)]
        me: bool,
        /// A one-time four-word code instead of the link
        #[arg(long)]
        code: bool,
        /// Hours until it expires
        #[arg(long, default_value_t = 24)]
        hours: u64,
    },
    /// Show the team link (anyone with it can join)
    Link {
        /// Replace it: the old link stops working
        #[arg(long)]
        new: bool,
    },
    /// Join a team with a team link (or an invite code)
    Join {
        invite: Option<String>,
        /// Your name in the team (default: $USER)
        #[arg(long = "as")]
        person: Option<String>,
        #[arg(long)]
        no_setup: bool,
    },
    /// Show the team's people and machines, and how each is reached
    Team,
    /// Remove a person (all their machines) or one machine from the team
    Remove { who: String },
    /// Leave the team on this machine
    Leave {
        /// Leave even if no teammate is online to hear it
        #[arg(long)]
        force: bool,
    },
    /// Use your own iroh relay for the team, so its machines never contact n0 (or `default` for n0's public relays)
    Relay { url: String },
    /// List messages waiting for your approval (when `inbound` is `hold`)
    Held,
    /// Deliver held messages
    Approve {
        ids: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// Drop held messages
    Reject {
        ids: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// Wire Reason Network into the agents on this machine and install the background service
    Setup {
        /// Show what would change without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Undo: remove Reason Network from every agent and remove the service
        #[arg(long)]
        remove: bool,
        /// Don't install the background service
        #[arg(long)]
        no_service: bool,
    },
    /// Check that everything is working
    Doctor,
    /// Update to the latest release (with the Reason CLI, `reason update` does it)
    Upgrade,
    /// Called by agents' hooks
    #[command(hide = true)]
    Hook {
        agent: String,
        event: String,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        doing: Option<String>,
    },
    /// Run the daemon in the foreground (normally started on demand or by the service)
    Daemon {
        /// Wait for a running daemon to exit instead of quitting (for service managers, and Windows' logon entry)
        #[arg(long)]
        service: bool,
    },
    /// Restart the daemon
    Stop,
    /// Wait for the next message to a named session, print it, and exit (for agents without Reason Network's tools, e.g.
    /// cloud agents: run it again after replying)
    Listen {
        /// The session's name; reachable as <you>/<name>
        #[arg(long = "as", default_value = "agent")]
        name: String,
        /// Give up after this many seconds
        #[arg(long, default_value_t = 600)]
        wait: u64,
    },
    /// Run the MCP server on stdio (what agents launch)
    Mcp,
    /// The menu bar icon (macOS; the background service starts it)
    #[command(hide = true)]
    Tray,
}

#[tokio::main]
async fn main() -> Result<()> {
    migrate::as_ours();
    let cmd = Cli::parse().cmd;
    paths::migrate();
    match cmd {
        Some(Cmd::Daemon { service }) => daemon::run(service).await,
        Some(Cmd::Mcp) => mcp::run().await,
        // Blocks this (the main) thread with AppKit's event loop; its daemon calls run on the runtime's workers.
        Some(Cmd::Tray) => tray::run(),
        Some(Cmd::Hook { agent, event, session, state, doing }) => {
            hook::run(&agent, &event, session, state, doing).await;
            // Don't wait on anything left behind (a stuck stdin read): the agent is waiting on us.
            std::process::exit(0)
        }
        Some(cmd) => cli(cmd).await,
        // `reason network` alone: get set up if needed, else show who's around.
        None => {
            let in_team = Link::new(None).call::<TeamInfo>(Request::Team).await.is_ok();
            if !in_team && wizard::interactive() {
                return wizard::guided(wizard::onboard(None, None, None, None)).await;
            }
            if !in_team {
                println!(
                    "Not in a team yet. Run `{CLI}` in a terminal to get set up (or `{CLI} init <team>` / `{CLI} join <code>`)."
                );
                return Ok(());
            }
            cli(Cmd::Who { filter: None, all: false, json: false }).await
        }
    }
}

/// The agent session this shell belongs to, if it runs inside one.
/// A session driven from the command line (`reason network listen --as <id>`, or `terminal` for plain-terminal sends): kept
/// alive like a hook-known session, so it stays reachable between commands.
async fn cli_session(link: &std::sync::Arc<Link>, id: &str, event: &str) -> Result<HookReply> {
    let req = Request::Hook(HookCall {
        agent: "cli".into(),
        event: event.into(),
        session_id: id.into(),
        cwd: Some(agent::cwd()),
        pid: None,
        mail: false,
        transcript: None,
        doing: None,
        name: Some(id.into()),
    });
    link.call(req).await
}

fn current_session() -> Option<String> {
    let agent = agent::detect_env()?;
    Some(format!("{agent}:{}", agent::env_session_id(agent)?))
}

fn default_person(p: Option<String>) -> String {
    p.unwrap_or_else(|| config::Config::load().person())
}

async fn cli(cmd: Cmd) -> Result<()> {
    // Like other command-line tools, stop quietly when whatever reads our output goes away. (`listen` marks
    // messages read only after printing them, so nothing is lost.)
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL)
    };
    let link = Link::new(None);
    match cmd {
        Cmd::Who { filter, all, json } => {
            let d: Directory = link.call(Request::Who { me: current_session(), filter, all }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&d)?);
            } else {
                print!("{}", render::directory(&d, false, render::Paint::terminal()));
            }
        }
        Cmd::Send { to, message, reply_to, fyi, notify, from, files } => {
            let mut body = message.join(" ");
            if message.is_empty() || body == "-" {
                body.clear();
                std::io::stdin().read_to_string(&mut body)?;
            }
            if body.trim().is_empty() {
                bail!("empty message");
            }
            let files = files::read(&files, &std::env::current_dir()?)?;
            // Messages come from agent sessions: `--from`'s, this agent's, or from a plain terminal, `<you>/terminal`.
            // An agent shell Reason Network doesn't know (its tools aren't loaded) counts as a terminal.
            let send = |from: String| Request::Send {
                from,
                to: to.clone(),
                body: body.clone(),
                reply_to: reply_to.clone(),
                wake: !fyi,
                notify,
                files: files.clone(),
            };
            // As the terminal, a group send has to come from a person sitting at it, not an agent's shell or a script.
            let as_terminal = || {
                if is_group(&to) && !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
                    bail!("only a person at a terminal sends to a group; from a script, add `--from <you>/terminal`");
                }
                Ok(())
            };
            let explicit = from.is_some();
            let r: SendResult = match from.or_else(current_session) {
                Some(f) => match link.call(send(f)).await {
                    Err(e) if !explicit && e.to_string().contains("isn't registered") => {
                        as_terminal()?;
                        cli_session(&link, "terminal", "stop").await?;
                        link.call(send("cli:terminal".into())).await?
                    }
                    r => r?,
                },
                None => {
                    as_terminal()?;
                    cli_session(&link, "terminal", "stop").await?;
                    link.call(send("cli:terminal".into())).await?
                }
            };
            print!("{}", render::send_result(&r));
        }
        Cmd::Status { doing, name } => {
            let Some(key) = current_session() else {
                bail!("run this from inside an agent session's shell");
            };
            let doing = (!doing.is_empty()).then(|| doing.join(" "));
            let r: Registered = link.call(Request::SetStatus { key, doing, name }).await?;
            println!("You are {}.", r.address);
        }
        Cmd::Init { team, person, no_setup, relay: None } if !no_setup && wizard::interactive() => {
            wizard::guided(wizard::onboard(Some(true), team, None, person)).await?
        }
        Cmd::Join { invite, person, no_setup } if !no_setup && wizard::interactive() => {
            wizard::guided(wizard::onboard(Some(false), None, invite, person)).await?
        }
        Cmd::Invite { me: false, code: false, .. } if wizard::interactive() => wizard::guided(wizard::invite()).await?,
        Cmd::Link { new } => {
            let l: LinkInfo = link.call(Request::Link { new }).await?;
            let copied = wizard::interactive() && wizard::copy(&l.url);
            println!("Team link for {}{}:\n\n{}\n", l.team, if copied { " (on your clipboard)" } else { "" }, l.url);
            println!("Anyone with it can join: it opens a page with one command. `{CLI} link --new` replaces it.");
            if managed() {
                println!("Teammates in your Reason workspace just run `reason network`; the link is for anyone else.");
            }
        }
        Cmd::Invite { me: false, code: false, .. } => Box::pin(cli(Cmd::Link { new: false })).await?,
        Cmd::Setup { dry_run: false, remove: false, no_service: false } if wizard::interactive() => {
            wizard::guided(wizard::setup_agents()).await?
        }
        Cmd::Init { team, person, no_setup, relay } => {
            let Some(team) = team else {
                bail!("name the team: `{CLI} init <team>` (or run it in a terminal to be asked)")
            };
            let t: TeamInfo =
                link.call(Request::CreateTeam { name: team, person: default_person(person), relay }).await?;
            wait_exit(daemon_pid()).await;
            println!("Created team {}. You are {} on {}.", t.name, t.person, t.machine);
            if !no_setup {
                println!();
                setup::run(false, false, false).await?;
            }
            println!("\nNext: `{CLI} link` prints the team link to send your teammates.");
        }
        Cmd::Join { invite, person, no_setup } => {
            let Some(invite) = invite else { bail!("give the invite code: `{CLI} join <code>`") };
            let t: TeamInfo = link.call(Request::Join { ticket: invite, person: default_person(person) }).await?;
            wait_exit(daemon_pid()).await;
            println!("Joined team {} as {} on {}.", t.name, t.person, t.machine);
            if !no_setup {
                println!();
                setup::run(false, false, false).await?;
            }
            // Give the new daemon a moment to reach teammates, then show them.
            let fresh = Link::new(None);
            for _ in 0..20 {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if let Ok(t) = fresh.call::<TeamInfo>(Request::Team).await
                    && t.machines.iter().any(|m| !m.is_me && m.online)
                {
                    break;
                }
            }
            if let Ok(t) = fresh.call::<TeamInfo>(Request::Team).await {
                print!("\n{}", render::team(&t));
            }
        }
        Cmd::Invite { me, hours, .. } => {
            let i: InviteInfo = link.call(Request::Invite { me, hours: hours.clamp(1, 24 * 30) }).await?;
            let who = if me { "another machine of yours" } else { "a teammate" };
            println!("Invite code for {who}: {}\n", i.code);
            let [unix, windows] = install_lines(&i.code);
            if managed() && !me {
                println!("Teammates in your Reason workspace just run `reason network`. Anyone else:\n");
            }
            println!("They run this, which installs Reason Network (no account needed) and joins the team:\n");
            println!("  {unix}\n");
            println!("On Windows, in PowerShell:\n\n  {windows}\n");
            println!(
                "(or `{AGENT_NAME} join {}` if it's installed). Works once, within {}h, while this machine is online.",
                i.code, i.hours
            );
            println!("If this machine will be offline, use the long form instead: {AGENT_NAME} join {}", i.ticket);
            if me {
                println!(
                    "\nFor a cloud agent of yours (Grok Bot, Dots, Muse, …), send it: join my Reason Network team as {}: {}",
                    default_person(None),
                    join_url(&i.ticket)
                );
            }
        }
        Cmd::Listen { name, wait } => {
            let id = util::slug(&name);
            if id.is_empty() {
                bail!("a session name needs letters or digits");
            }
            let address = cli_session(&link, &id, "stop").await?.address.unwrap_or_default();
            let key = format!("cli:{id}");
            let i: Inbox = link.call(Request::Inbox { key: key.clone(), wait_secs: wait, peek: true }).await?;
            if i.messages.is_empty() && wait == 0 {
                println!("No messages. You are {address}.");
            } else if i.messages.is_empty() {
                println!("No messages in {wait}s. You are {address}; run this again to keep listening.");
            } else {
                println!("{}", i.text);
                let ids = i.messages.iter().map(|m| m.id.clone()).collect();
                link.call::<serde_json::Value>(Request::Delivered { key, ids }).await?;
                // Busy until it listens again.
                let _ = cli_session(&link, &id, "prompt").await;
            }
        }
        Cmd::Team => {
            let t: TeamInfo = link.call(Request::Team).await?;
            print!("{}", render::team(&t));
        }
        Cmd::Remove { who } => println!("{}", link.call::<String>(Request::Remove { who }).await?),
        Cmd::Leave { force } => {
            println!("{}", link.call::<String>(Request::Leave { force }).await?);
            wait_exit(daemon_pid()).await;
        }
        Cmd::Relay { url } => {
            let url = (url != "default").then_some(url);
            println!("{}", link.call::<String>(Request::SetRelay { url }).await?);
            wait_exit(daemon_pid()).await;
        }
        Cmd::Held => {
            let msgs: Vec<Message> = link.call(Request::Held).await?;
            if msgs.is_empty() {
                println!("Nothing held.");
            } else {
                print!("{}", render::for_human(&msgs));
                println!("`{CLI} approve <id>` delivers, `{CLI} reject <id>` drops (or --all).");
            }
        }
        Cmd::Approve { ids, all } | Cmd::Reject { ids, all } if ids.is_empty() && !all => {
            bail!("give message ids, or --all")
        }
        Cmd::Approve { ids, .. } => {
            let n: usize = link.call(Request::Settle { ids, release: true }).await?;
            println!("Delivered {n}.");
        }
        Cmd::Reject { ids, .. } => {
            let n: usize = link.call(Request::Settle { ids, release: false }).await?;
            println!("Dropped {n}.");
        }
        Cmd::Setup { dry_run, remove, no_service } => setup::run(dry_run, remove, no_service).await?,
        Cmd::Doctor => setup::doctor().await?,
        Cmd::Upgrade => {
            if managed() {
                bail!("the network engine updates with the reason CLI: update reason, then run `{CLI} setup`");
            }
            let dir = util::current_exe()?.parent().map(|d| d.to_path_buf()).unwrap_or_default();
            let exe = dir.join(format!("{AGENT_NAME}{}", std::env::consts::EXE_SUFFIX));
            let waiting = migrate::waiting(&exe);
            if !installer(&dir).status()?.success() {
                bail!("upgrade failed");
            }
            if waiting {
                migrate::keep_waiting_copy(&exe);
            }
            stop_daemon().await;
            println!("The daemon restarted on the new version. Restart agent sessions to update their tools.");
        }
        Cmd::Stop => {
            if !stop_daemon().await {
                println!("The daemon is not running.");
            }
        }
        Cmd::Daemon { .. } | Cmd::Mcp | Cmd::Hook { .. } | Cmd::Tray => unreachable!("handled in main"),
    }
    Ok(())
}

/// The running daemon's pid, from its lock file.
fn daemon_pid() -> Option<u32> {
    client::running_daemon()
}

/// Waits (briefly) for a daemon that is restarting to exit, so the next request reaches the new one. Call it
/// right after the reply: the daemon exits a moment later.
async fn wait_exit(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    for _ in 0..50 {
        if daemon_pid() != Some(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Asks a running daemon to exit and waits until it has. False if none was running.
pub async fn stop_daemon() -> bool {
    if ipc::connect().await.is_err() {
        return false;
    }
    let pid = daemon_pid();
    let _ = Link::existing().call::<serde_json::Value>(Request::Shutdown).await;
    wait_exit(pid).await;
    true
}
