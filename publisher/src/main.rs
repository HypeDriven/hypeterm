//! `hypeterm-publish` — mirror a terminal on this machine through a relay.

use clap::{Parser, Subcommand};
#[cfg(unix)]
use hypeterm_publish::daemon;
use hypeterm_publish::{api, crypto::KeyPair, pairing, protocol, pty, publish, session, state};
use portable_pty::CommandBuilder;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "hypeterm-publish",
    version,
    about = "Mirror a terminal on this machine through a Terminal Mirror Relay"
)]
struct Cli {
    /// Where the identity and device keys are kept. Defaults to the per-user config
    /// directory, or $HYPETERM_STATE.
    #[arg(long, global = true)]
    state_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register this machine with a relay, creating the owning identity if needed.
    Enroll {
        /// Relay base URL, e.g. https://hypeterm-relay.example.ts.net
        #[arg(long)]
        relay: String,
        /// Name this machine takes in the device list.
        #[arg(long)]
        name: Option<String>,
    },
    /// Register another device — a phone — as a client of this identity.
    ///
    /// The device shows its own public key and keeps its private half; this only
    /// vouches for it. Both parties act, which is what makes pairing mean something.
    Pair {
        /// The base64url public key the device displays.
        public_key: String,
        #[arg(long, default_value = "phone")]
        name: String,
    },
    /// Host a terminal and publish it. With no command, starts your shell.
    Run {
        /// Label subscribers see in the terminal list.
        #[arg(long)]
        label: Option<String>,
        /// Answers a subscriber's terminal-open request (relay spec §4.6). Set by the
        /// daemon when it hosts a terminal because a phone asked; not for typing by
        /// hand.
        #[arg(long, hide = true)]
        in_reply_to: Option<String>,
        /// Initial size, when there is no terminal to measure. Also set by the daemon.
        #[arg(long, hide = true)]
        cols: Option<u16>,
        #[arg(long, hide = true)]
        rows: Option<u16>,
        /// The command to host. Defaults to $SHELL, or %COMSPEC% on Windows.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Print a pairing code for a phone.
    ///
    /// The code lends the device this identity's authority for a few minutes so it can
    /// register itself. The device still signs its own registration challenge, so its
    /// private key never leaves it and this machine never sees it.
    PairCode {
        /// The relay address the *phone* should use, when it differs from this
        /// machine's. A relay behind a Tailscale sidecar is reached by its MagicDNS
        /// name from the tailnet and by something else from the host running it.
        #[arg(long)]
        url: Option<String>,
    },
    /// Allow, or stop allowing, a paired phone to ask this machine for a terminal.
    ///
    /// Off until you turn it on, and it is the switch that matters: the relay may say
    /// whatever it likes, but nothing spawns here unless this is set. A phone that can
    /// both open terminals and type into them can run anything you can.
    RemoteOpen {
        /// Allow it, capturing the shell and directory to use.
        #[arg(long, conflicts_with_all = ["disable", "status"])]
        enable: bool,
        /// Stop allowing it.
        #[arg(long, conflicts_with_all = ["enable", "status"])]
        disable: bool,
        /// Show the current policy.
        #[arg(long, conflicts_with_all = ["enable", "disable"])]
        status: bool,
        #[command(flatten)]
        settings: RemoteOpenSettings,
    },
    /// Run the process that mirrors every terminal on this machine.
    ///
    /// Started automatically by `run`; this is here for stopping it, watching it, and
    /// finding its log. A device may hold only one publisher connection to the relay
    /// (relay spec §6.1), so one process has to own it on behalf of them all.
    Daemon {
        /// Start the daemon and return, leaving it running. What `run` uses.
        #[arg(long, conflicts_with_all = ["foreground", "stop", "status"])]
        detach: bool,
        /// Run it here, in this terminal, with its log on stderr.
        #[arg(long, conflicts_with_all = ["stop", "status"])]
        foreground: bool,
        /// Ask a running daemon to stand down. Refused while it is mirroring anything.
        #[arg(long, conflicts_with = "status")]
        stop: bool,
        /// Say whether one is running, and where its log is.
        #[arg(long)]
        status: bool,
    },
    /// List the terminals this identity is publishing.
    List,
    /// Show what this machine has enrolled.
    Status,
}

/// What `remote-open --enable` records. One value rather than four loose arguments,
/// because every one of them is only meaningful alongside the others.
#[derive(clap::Args, Debug)]
struct RemoteOpenSettings {
    /// Program to host, defaulting to $SHELL. Taken as argv, never as a shell
    /// string: nothing on this path is passed to `sh -c`.
    #[arg(long, num_args = 1.., requires = "enable")]
    shell: Option<Vec<String>>,
    /// Working directory for terminals opened this way. Defaults to $HOME.
    #[arg(long, requires = "enable")]
    cwd: Option<String>,
    /// How many terminals a phone may have open here at once.
    #[arg(long, requires = "enable")]
    max: Option<u32>,
    /// Whether a terminal opened this way also gets a window on this machine's own
    /// screen: `auto` to open one when this machine has a display and an emulator to
    /// open it with, `never` to always host headlessly, or the emulator command to
    /// use, which the hosting command is appended to — `--window konsole -e`.
    #[arg(long, num_args = 1.., requires = "enable", value_name = "auto|never|COMMAND")]
    window: Option<Vec<String>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("HYPETERM_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    hypeterm_publish::tls::ensure_provider();

    let cli = Cli::parse();
    let path = cli.state_file.clone().unwrap_or_else(state::default_path);

    let outcome = match cli.command {
        Command::Enroll { relay, name } => enroll(&path, &relay, name).await,
        Command::Pair { public_key, name } => pair(&path, &public_key, &name).await,
        Command::Run {
            label,
            in_reply_to,
            cols,
            rows,
            command,
        } => run(&path, label, command, in_reply_to, cols, rows).await,
        Command::PairCode { url } => pair_code(&path, url).await,
        Command::Daemon {
            detach,
            foreground,
            stop,
            status,
        } => daemon_command(&path, detach, foreground, stop, status).await,
        Command::RemoteOpen {
            enable,
            disable,
            status,
            settings,
        } => remote_open(&path, enable, disable, status, settings),
        Command::List => list_terminals(&path).await,
        Command::Status => status(&path),
    };

    if let Err(message) = outcome {
        eprintln!("hypeterm-publish: {message}");
        std::process::exit(1);
    }
}

fn load(path: &std::path::Path) -> Result<state::State, String> {
    state::load(path).map_err(|e| e.to_string())
}

async fn enroll(path: &std::path::Path, relay: &str, name: Option<String>) -> Result<(), String> {
    let mut stored = load(path)?;
    let client = api::Client::new(relay).map_err(|e| e.to_string())?;

    // The identity owns everything and its key never leaves this machine. Reusing an
    // existing one matters: the identity ID is a fingerprint of the key, so a second
    // key would be a second, separate world with none of the paired devices in it.
    let identity = match stored.identity_key() {
        Some(key) => {
            println!("using the identity already on this machine");
            key
        }
        None => {
            let key = KeyPair::generate();
            stored.set_identity_key(&key);
            key
        }
    };

    // Registration is idempotent by key, so re-running enroll is safe.
    let registered = client
        .register_identity(&identity)
        .await
        .map_err(|e| format!("registering the identity: {e}"))?;
    stored.identity_id = registered.identity_id.clone();

    let device = match stored.device_key() {
        Some(key) => key,
        None => {
            let key = KeyPair::generate();
            stored.set_device_key(&key);
            key
        }
    };

    let token = client
        .identity_token(&identity)
        .await
        .map_err(|e| format!("authenticating as the identity: {e}"))?;

    let device_name = name.unwrap_or_else(|| {
        std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "this machine".into())
    });

    let registered_device = client
        .register_device(
            &token.access_token,
            &registered.identity_id,
            &device.public_key_base64(),
            Some(&device),
            &device_name,
            "publisher",
        )
        .await
        .map_err(|e| format!("registering this machine as a publisher: {e}"))?;

    stored.relay_url = client.base_url().to_string();
    stored.device_id = registered_device.device_id.clone();
    stored.device_name = device_name.clone();
    state::save(path, &stored).map_err(|e| e.to_string())?;

    println!("enrolled with {}", stored.relay_url);
    println!("  identity  {}", stored.identity_id);
    println!("  device    {} ({device_name})", stored.device_id);
    println!("  state     {}", path.display());
    println!();
    println!("Pair a phone with:  hypeterm-publish pair <its public key>");
    println!("Publish a terminal: hypeterm-publish run");
    Ok(())
}

async fn pair(path: &std::path::Path, public_key: &str, name: &str) -> Result<(), String> {
    let stored = load(path)?;
    let identity = stored
        .identity_key()
        .ok_or("this machine has no identity yet; run `hypeterm-publish enroll` first")?;
    if stored.relay_url.is_empty() {
        return Err("no relay is configured; run `hypeterm-publish enroll` first".into());
    }

    let client = api::Client::new(&stored.relay_url).map_err(|e| e.to_string())?;
    let token = client
        .identity_token(&identity)
        .await
        .map_err(|e| format!("authenticating as the identity: {e}"))?;

    // A phone holds its own private key, so it — not this machine — must sign the
    // registration challenge. Without that signature this would be a claim that
    // anyone could make about anyone's key.
    let device = client
        .register_device(
            &token.access_token,
            &stored.identity_id,
            public_key,
            None,
            name,
            "client",
        )
        .await;

    match device {
        Ok(device) => {
            println!("paired {name}");
            println!("  identity_id  {}", stored.identity_id);
            println!("  device_id    {}", device.device_id);
            Ok(())
        }
        Err(api::ApiError::Malformed(message)) if message.contains("private key") => Err(
            // Being explicit here rather than failing obscurely: this is the one part
            // of pairing that cannot be done from this side alone.
            "pairing a device that holds its own key needs a signature from that key.\n\
             The relay requires the device to sign its own registration challenge, so \
             the phone has to complete this step itself.\n\
             See `hypeterm-publish pair --help`."
                .into(),
        ),
        Err(error) => Err(format!("registering the device: {error}")),
    }
}

async fn run(
    path: &std::path::Path,
    label: Option<String>,
    command: Vec<String>,
    in_reply_to: Option<String>,
    requested_cols: Option<u16>,
    requested_rows: Option<u16>,
) -> Result<(), String> {
    let stored = load(path)?;
    let device_key = stored
        .device_key()
        .ok_or("this machine is not enrolled; run `hypeterm-publish enroll --relay <url>`")?;
    if stored.relay_url.is_empty() || stored.device_id.is_empty() {
        return Err(
            "this machine is not enrolled; run `hypeterm-publish enroll --relay <url>`".into(),
        );
    }

    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
    // With no tty, `terminal_size` answers 80x24 — which is nobody's phone. A terminal
    // hosted because a subscriber asked starts at the size the subscriber asked for.
    let (cols, rows) = match (requested_cols, requested_rows) {
        (Some(cols), Some(rows)) => (cols, rows),
        _ => terminal_size(),
    };

    let mut builder = if command.is_empty() {
        pty::default_shell()
    } else {
        let mut builder = CommandBuilder::new(&command[0]);
        for argument in &command[1..] {
            builder.arg(argument);
        }
        builder
    };
    // The mirror renders an xterm-256color-compatible screen (client spec §8.1), so
    // tell the hosted shell that is what it is talking to.
    builder.env("TERM", "xterm-256color");
    if let Ok(dir) = std::env::current_dir() {
        builder.cwd(dir);
    }

    let (terminal, mut output) = pty::spawn(builder, cols, rows).map_err(|e| e.to_string())?;

    // The label is the only thing distinguishing one row from another in the client's
    // terminal list, so several shells opened the same way must not all read the same.
    // The working directory is what usually tells them apart to the person who opened
    // them; the process id disambiguates the rest.
    let label = label
        .map(|given| decorate_label(&given))
        .unwrap_or_else(|| {
            let host = std::env::var("HOSTNAME")
                .or_else(|_| std::env::var("COMPUTERNAME"))
                .unwrap_or_else(|_| "terminal".into());
            if command.is_empty() {
                decorate_label(&host)
            } else {
                decorate_label(&format!("{host}: {}", command.join(" ")))
            }
        });

    let spec = session::TerminalSpec {
        in_reply_to,
        // Fresh per hosted shell, and never derived from the pid, the tty or the
        // directory: the relay deduplicates opens by (device, local_ref), so two
        // shells sharing one would be spliced onto a single offset stream.
        local_ref: uuid::Uuid::new_v4().to_string(),
        label,
        cols,
        rows,
        term: "xterm-256color".into(),
    };

    let mirror = match connect_mirror(path, &stored, device_key, spec).await {
        Ok(mirror) => Some(mirror),
        Err(reason) => {
            // The shell is hosted either way. Mirroring is what this tool is for, but
            // it is not worth refusing someone their terminal over.
            notice(interactive, &format!("not mirroring: {reason}"));
            None
        }
    };

    // Raw mode so the hosted shell, not this process's terminal driver, interprets
    // every key. Restored on the way out however the session ends.
    let raw = interactive && crossterm::terminal::enable_raw_mode().is_ok();
    let result = pump(&terminal, &mut output, mirror, interactive, cols, rows).await;
    if raw {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    // After raw mode the cursor can be anywhere; leave the shell a clean line.
    if interactive {
        println!();
    }
    result
}

/// Attaches this terminal to whatever carries frames to the relay.
///
/// On Unix that is always the machine's daemon, started here if it is not already
/// running. It is never this process directly: a device may hold only one publisher
/// connection (relay spec §6.1), so a second one would supersede the daemon's and
/// every *other* mirrored tab on the machine would go dark because of one terminal.
#[cfg(unix)]
async fn connect_mirror(
    state_file: &std::path::Path,
    stored: &state::State,
    _device_key: KeyPair,
    spec: session::TerminalSpec,
) -> Result<publish::Mirror, String> {
    let paths = hypeterm_publish::ipc::Paths::for_device(&stored.relay_url, &stored.device_id)?;
    let state_file = std::fs::canonicalize(state_file).unwrap_or_else(|_| state_file.to_path_buf());
    let (requests, events) = daemon::attach(&paths, &state_file, spec).await?;
    Ok(publish::start(requests, events))
}

/// Windows has no daemon (see `daemon`'s module documentation), so this process owns
/// the device's one publisher connection and mirrors the one terminal it hosts.
#[cfg(not(unix))]
async fn connect_mirror(
    _state_file: &std::path::Path,
    stored: &state::State,
    device_key: KeyPair,
    spec: session::TerminalSpec,
) -> Result<publish::Mirror, String> {
    // The second half is the stream of terminals a subscriber has asked this machine
    // to open. Nothing here can honour one — hosting a requested terminal means
    // starting another `run`, which is the daemon's job, and there is no daemon on
    // Windows — so the capability is not claimed and every request is declined. Saying
    // no is the honest answer; advertising a promise this build cannot keep is not.
    let (link, _open_requests) = session::start(
        session::Config {
            relay_url: stored.relay_url.clone(),
            device_id: stored.device_id.clone(),
            identity_key: stored.identity_key(),
            allow_remote_open: false,
        },
        device_key,
    );
    let terminal = link
        .open(spec)
        .await
        .ok_or("the relay connection ended before this terminal could open")?;
    let (requests, events) = publish::direct(terminal);
    Ok(publish::start(requests, events))
}

/// Moves bytes between the local terminal, the pseudo-terminal and the mirror.
async fn pump(
    terminal: &pty::Pty,
    output: &mut pty::PtyOutput,
    mirror: Option<publish::Mirror>,
    interactive: bool,
    mut cols: u16,
    mut rows: u16,
) -> Result<(), String> {
    let mut stdin = tokio::io::stdin();
    let mut stdin_buffer = vec![0u8; 4096];
    let mut shutting_down = false;
    let mut shell_gone = false;
    let mut deadline: Option<tokio::time::Instant> = None;
    // Built once, outside the loop. A freshly created signal stream does not see a
    // signal that was delivered before it existed, so rebuilding it on every pass
    // would quietly lose the SIGHUP that a closed terminal tab sends while another arm
    // was running — and losing that is exactly how a terminal gets left open at the
    // relay with nothing behind it.
    let mut signals = Signals::new();
    // Cleared when mirroring ends for good. The shell keeps running: losing the mirror
    // is not a reason to take away the terminal someone is working in.
    let mut mirror = mirror;
    let mut size_check = tokio::time::interval(Duration::from_millis(500));
    size_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // Not biased. Output was polled first here once, on the reasoning that it is
        // what everything else is in service of — but a shell producing output without
        // pause then starved the arm carrying keystrokes towards it, and a terminal
        // nobody can type into is not a mirror of anything.
        tokio::select! {
            chunk = output.chunks.recv(), if !shell_gone => {
                let Some(chunk) = chunk else {
                    // The hosted shell has gone. Close the terminal at the relay
                    // before leaving, or it stays open for the relay's whole grace
                    // period, listed on the phone, swallowing anything typed into it.
                    shell_gone = true;
                    match mirror.as_mut() {
                        Some(active) if !shutting_down => {
                            shutting_down = true;
                            active.begin_shutdown();
                            deadline = Some(tokio::time::Instant::now() + SHUTDOWN_GRACE);
                        }
                        Some(_) => {}
                        None => return Ok(()),
                    }
                    continue;
                };
                if interactive {
                    let mut out = std::io::stdout();
                    let _ = out.write_all(&chunk);
                    let _ = out.flush();
                }
                if let Some(active) = mirror.as_ref()
                    && !active.publish(chunk).await
                {
                    mirror = None;
                    notice(interactive, "mirroring stopped; this terminal still works");
                }
            }

            notice_received = async {
                match mirror.as_mut() {
                    Some(active) => active.notices.recv().await,
                    // Never resolves, so the arm is simply inert once mirroring ends.
                    None => std::future::pending().await,
                }
            } => {
                match notice_received {
                    Some(publish::Notice::Input(bytes)) => {
                        if !terminal.write(bytes).await {
                            return Ok(());
                        }
                    }
                    Some(publish::Notice::Resize { cols: c, rows: r }) => {
                        // With a local terminal attached, its size is the real one and
                        // a subscriber's request would fight it (spec §6.5 leaves the
                        // decision to the publisher). Headless, the request is all
                        // there is, so it wins.
                        if interactive {
                            tracing::info!(cols = c, rows = r, "declining a resize; this terminal follows its own window");
                        } else {
                            cols = c;
                            rows = r;
                            terminal.resize(c, r);
                            if let Some(active) = mirror.as_ref() {
                                active.resize(c, r).await;
                            }
                        }
                    }
                    Some(publish::Notice::Ended(reason)) => {
                        // Mirroring is over, but the shell is not. Returning here would
                        // drop the pseudo-terminal, hang up the shell, and take the
                        // user's work with it because of a network problem.
                        tracing::info!(%reason, "mirroring ended");
                        if shutting_down {
                            return Ok(());
                        }
                        mirror = None;
                        notice(interactive, &format!("{reason}; this terminal still works"));
                    }
                    None => {
                        if shutting_down {
                            return Ok(());
                        }
                        mirror = None;
                    }
                }
            }

            read = tokio::io::AsyncReadExt::read(&mut stdin, &mut stdin_buffer), if interactive => {
                match read {
                    Ok(0) => {
                        // Local end of input. The hosted shell may still be running for
                        // a subscriber, so this is not the end of the session.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    Ok(n) => {
                        if !terminal.write(stdin_buffer[..n].to_vec()).await {
                            return Ok(());
                        }
                    }
                    Err(error) => return Err(format!("reading the keyboard: {error}")),
                }
            }

            // A terminal that is merely abandoned stays "open" at the relay for its
            // whole reconnect grace period. During that time a subscriber can still
            // attach to it and type into nothing. Closing properly on a signal is what
            // keeps the terminal list honest — and SIGHUP is in the set because that
            // is what closing a terminal tab sends.
            _ = signals.next(), if !shutting_down => {
                shutting_down = true;
                if let Some(active) = mirror.as_mut() {
                    active.begin_shutdown();
                    deadline = Some(tokio::time::Instant::now() + SHUTDOWN_GRACE);
                } else {
                    return Ok(());
                }
            }

            _ = async { tokio::time::sleep_until(deadline.unwrap()).await },
                    if deadline.is_some() => {
                // Nothing confirmed in time. Leaving is still better than hanging: the
                // close is already on its way and the daemon outlives this process.
                return Ok(());
            }

            _ = size_check.tick(), if interactive => {
                let (new_cols, new_rows) = terminal_size();
                if (new_cols, new_rows) != (cols, rows) {
                    cols = new_cols;
                    rows = new_rows;
                    terminal.resize(cols, rows);
                    if let Some(active) = mirror.as_ref() {
                        active.resize(cols, rows).await;
                    }
                }
            }
        }
    }
}

/// Tells the user something about the mirror, on their own terminal.
///
/// stderr rather than the log: this is addressed to whoever is sitting in front of the
/// terminal, and the log is off by default.
fn notice(interactive: bool, message: &str) {
    if interactive {
        eprintln!("\r\n[hypeterm] {message}\r");
    } else {
        eprintln!("[hypeterm] {message}");
    }
}

/// How long to wait for a clean close to be acknowledged before giving up.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// The signals that mean "stop", held open for the life of the session.
///
/// Held rather than created on demand: a signal stream only reports what arrives after
/// it exists, so one built inside a `select!` arm loses anything delivered while a
/// different arm was running.
struct Signals {
    #[cfg(unix)]
    terminate: Option<tokio::signal::unix::Signal>,
    /// Closing a terminal tab sends SIGHUP, which is by far the most common way one of
    /// these ends. Without it the terminal was left open at the relay for its whole
    /// grace period, listed on the phone, swallowing anything typed into it.
    #[cfg(unix)]
    hangup: Option<tokio::signal::unix::Signal>,
}

impl Signals {
    fn new() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Self {
                terminate: signal(SignalKind::terminate()).ok(),
                hangup: signal(SignalKind::hangup()).ok(),
            }
        }
        #[cfg(not(unix))]
        Self {}
    }

    /// Resolves on the first signal asking this process to stop.
    async fn next(&mut self) {
        #[cfg(unix)]
        {
            let terminated = async {
                match self.terminate.as_mut() {
                    Some(stream) => {
                        stream.recv().await;
                    }
                    None => std::future::pending().await,
                }
            };
            let hung_up = async {
                match self.hangup.as_mut() {
                    Some(stream) => {
                        stream.recv().await;
                    }
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = terminated => {}
                _ = hung_up => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(unix)]
async fn daemon_command(
    path: &std::path::Path,
    detach: bool,
    foreground: bool,
    stop: bool,
    status: bool,
) -> Result<(), String> {
    let stored = load(path)?;
    if stored.relay_url.is_empty() || stored.device_id.is_empty() {
        return Err(
            "this machine is not enrolled; run `hypeterm-publish enroll --relay <url>`".into(),
        );
    }
    let paths = hypeterm_publish::ipc::Paths::for_device(&stored.relay_url, &stored.device_id)?;
    let state_file = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    if status {
        match hypeterm_publish::daemon::probe(&paths).await {
            Some(running) => println!(
                "a daemon is running (pid {}, build {}) on {}",
                running.pid,
                running.build,
                paths.socket.display()
            ),
            None => println!("no daemon is running for this device"),
        }
        println!("log        {}", paths.log.display());
        return Ok(());
    }

    if stop {
        return match hypeterm_publish::daemon::stop(&paths).await? {
            true => {
                println!("the daemon is standing down");
                Ok(())
            }
            false => {
                println!("no daemon is running for this device");
                Ok(())
            }
        };
    }

    if detach {
        // The first of two hops: this process exists only so the caller has something
        // short-lived to reap, and so the daemon proper is reparented away from the
        // terminal tab that asked for it.
        return hypeterm_publish::daemon::respawn_detached(&state_file, &paths.log);
    }

    let _ = foreground;
    let device_key = stored
        .device_key()
        .ok_or("this machine is not enrolled; run `hypeterm-publish enroll --relay <url>`")?;
    let remote_open = state::RemoteOpenConfig {
        enabled: stored.remote_open.enabled,
        shell: stored.remote_open.shell.clone(),
        cwd: stored.remote_open.cwd.clone(),
        max_terminals: stored.remote_open.max_terminals,
        window: stored.remote_open.window.clone(),
    };
    hypeterm_publish::daemon::serve(
        paths,
        session::Config {
            relay_url: stored.relay_url.clone(),
            device_id: stored.device_id.clone(),
            identity_key: stored.identity_key(),
            allow_remote_open: stored.remote_open.enabled,
        },
        device_key,
        remote_open,
        path.to_path_buf(),
    )
    .await
}

#[cfg(not(unix))]
async fn daemon_command(
    _path: &std::path::Path,
    _detach: bool,
    _foreground: bool,
    _stop: bool,
    _status: bool,
) -> Result<(), String> {
    Err(
        "there is no mirroring daemon on Windows; `run` publishes directly here, one \
         terminal at a time. A shell inside WSL can run the Linux build and mirror \
         several at once."
            .into(),
    )
}

/// Appends the working directory and process id, so two terminals started the same
/// way are still tellable apart in a list on a phone.
fn decorate_label(base: &str) -> String {
    let directory = std::env::current_dir()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_default();
    let pid = std::process::id();
    if directory.is_empty() {
        format!("{base} ({pid})")
    } else {
        format!("{base}: {directory} ({pid})")
    }
}

fn terminal_size() -> (u16, u16) {
    crossterm::terminal::size()
        .ok()
        .filter(|(c, r)| *c > 0 && *r > 0)
        .unwrap_or((80, 24))
}

async fn pair_code(path: &std::path::Path, url: Option<String>) -> Result<(), String> {
    let stored = load(path)?;
    let identity = stored
        .identity_key()
        .ok_or("this machine has no identity yet; run `hypeterm-publish enroll` first")?;
    if stored.relay_url.is_empty() {
        return Err("no relay is configured; run `hypeterm-publish enroll` first".into());
    }

    let client = api::Client::new(&stored.relay_url).map_err(|e| e.to_string())?;
    let token = client
        .identity_token(&identity)
        .await
        .map_err(|e| format!("authenticating as the identity: {e}"))?;

    let device_url = url.unwrap_or_else(|| stored.relay_url.clone());
    let code = pairing::encode(&pairing::PairingCode {
        server_url: device_url.clone(),
        identity_id: stored.identity_id.clone(),
        identity_token: token.access_token,
    });

    println!("{code}");
    println!();
    println!("The phone will connect to {device_url}");
    println!(
        "Paste that into the phone's pairing screen. It expires in {} minutes, and it \n\
         carries this identity's authority until then — treat it like a password.",
        (token.expires_in / 60).max(1)
    );
    Ok(())
}

async fn list_terminals(path: &std::path::Path) -> Result<(), String> {
    let stored = load(path)?;
    let identity = stored
        .identity_key()
        .ok_or("this machine has no identity yet; run `hypeterm-publish enroll` first")?;
    let client = api::Client::new(&stored.relay_url).map_err(|e| e.to_string())?;
    let token = client
        .identity_token(&identity)
        .await
        .map_err(|e| format!("authenticating as the identity: {e}"))?;
    let terminals = client
        .terminals(&token.access_token)
        .await
        .map_err(|e| format!("listing terminals: {e}"))?;

    if terminals.is_empty() {
        println!("no terminals are being published");
        return Ok(());
    }
    for terminal in terminals {
        println!(
            "{}  {:<24} {:>3}x{:<3} {}{}",
            terminal.terminal_id,
            terminal.label,
            terminal.cols.unwrap_or(0),
            terminal.rows.unwrap_or(0),
            terminal.state,
            if terminal.accepts_input {
                ""
            } else {
                " (read only)"
            },
        );
    }
    Ok(())
}

/// The machine owner's own switch for phone-initiated terminals (relay spec §4.6).
///
/// The shell and directory are captured here, from the interactive shell doing the
/// enabling, rather than read later from the daemon: the daemon inherits whichever
/// environment first started it and runs from `/`, so "your shell, in your home
/// directory" has to be recorded at the moment somebody means it.
fn remote_open(
    path: &std::path::Path,
    enable: bool,
    disable: bool,
    show: bool,
    settings: RemoteOpenSettings,
) -> Result<(), String> {
    let mut stored = load(path)?;

    if disable {
        stored.remote_open.enabled = false;
        state::save(path, &stored).map_err(|e| e.to_string())?;
        println!("remote open: off");
        println!("restart the daemon to apply: hypeterm-publish daemon --stop");
        return Ok(());
    }

    if enable {
        let RemoteOpenSettings {
            shell,
            cwd,
            max,
            window,
        } = settings;
        let argv = match shell {
            Some(argv) if !argv.is_empty() => argv,
            _ => vec![std::env::var("SHELL").map_err(|_| {
                "no $SHELL to capture; pass --shell <program> [args...]".to_string()
            })?],
        };
        let directory = match cwd {
            Some(dir) => dir,
            None => std::env::var("HOME").unwrap_or_default(),
        };
        stored.remote_open.enabled = true;
        stored.remote_open.shell = argv;
        stored.remote_open.cwd = directory;
        if let Some(max) = max {
            stored.remote_open.max_terminals = max.max(1);
        }
        if let Some(spec) = window {
            stored.remote_open.window = window_policy(spec)?;
        }
        state::save(path, &stored).map_err(|e| e.to_string())?;

        println!("remote open: on");
        println!("  shell     {:?}", stored.remote_open.shell);
        println!(
            "  directory {}",
            if stored.remote_open.cwd.is_empty() {
                "$HOME"
            } else {
                &stored.remote_open.cwd
            }
        );
        println!("  at most   {} terminals", stored.remote_open.max_terminals);
        println!(
            "  window    {}",
            describe_window(&stored.remote_open.window)
        );
        println!();
        // Said plainly, once, at the moment of consent. Somebody turning this on should
        // not have to infer what it means from a spec section.
        println!("A phone paired to this identity can now start shells on this machine.");
        println!("Combined with typing, that is the same reach as sitting here.");
        println!("Turn it off with: hypeterm-publish remote-open --disable");
        println!("restart the daemon to apply: hypeterm-publish daemon --stop");
        return Ok(());
    }

    let _ = show;
    println!(
        "remote open: {}",
        if stored.remote_open.enabled {
            "on"
        } else {
            "off"
        }
    );
    if stored.remote_open.enabled {
        println!("  shell     {:?}", stored.remote_open.shell);
        println!(
            "  directory {}",
            if stored.remote_open.cwd.is_empty() {
                "$HOME"
            } else {
                &stored.remote_open.cwd
            }
        );
        println!("  at most   {} terminals", stored.remote_open.max_terminals);
        println!(
            "  window    {}",
            describe_window(&stored.remote_open.window)
        );
    } else {
        println!("enable with: hypeterm-publish remote-open --enable");
    }
    Ok(())
}

/// `auto`, `never`, or an emulator argv.
fn window_policy(spec: Vec<String>) -> Result<state::WindowPolicy, String> {
    if spec.is_empty() {
        return Err("--window takes auto, never, or a terminal command".into());
    }
    if spec.len() == 1 {
        match spec[0].as_str() {
            "auto" => return Ok(state::WindowPolicy::Auto),
            "never" => return Ok(state::WindowPolicy::Never),
            _ => {}
        }
    }
    Ok(state::WindowPolicy::Command(spec))
}

/// Says what the policy will actually do, not just what it is called.
///
/// `auto` on its own leaves somebody guessing whether a window will appear, which is
/// the question this setting exists to answer. The detection runs here rather than in
/// the daemon, so it reports this session's display — near enough to be useful, and
/// worth re-reading after `daemon --stop` if the two ever disagree.
fn describe_window(policy: &state::WindowPolicy) -> String {
    match policy {
        state::WindowPolicy::Never => "never — hosted headlessly".into(),
        state::WindowPolicy::Command(argv) => argv.join(" "),
        #[cfg(unix)]
        state::WindowPolicy::Auto => match hypeterm_publish::daemon::window_command(policy) {
            Some(argv) => format!("auto — {}", argv.join(" ")),
            None => "auto — no display or no terminal emulator here, so headless".into(),
        },
        #[cfg(not(unix))]
        state::WindowPolicy::Auto => "auto".into(),
    }
}

fn status(path: &std::path::Path) -> Result<(), String> {
    let stored = load(path)?;
    if stored.relay_url.is_empty() {
        println!("not enrolled ({})", path.display());
        println!("run: hypeterm-publish enroll --relay <url>");
        return Ok(());
    }
    println!("relay      {}", stored.relay_url);
    println!("identity   {}", stored.identity_id);
    println!("device     {} ({})", stored.device_id, stored.device_name);
    println!("state      {}", path.display());
    println!("protocol   {}", protocol::SUBPROTOCOL_V2);
    println!(
        "remote open {}",
        if stored.remote_open.enabled {
            "on"
        } else {
            "off"
        }
    );
    #[cfg(unix)]
    if let Ok(paths) =
        hypeterm_publish::ipc::Paths::for_device(&stored.relay_url, &stored.device_id)
    {
        // A detached daemon writes nowhere anyone would look, so say where. It is the
        // one part of this that a person cannot see running.
        let running = matches!(
            hypeterm_publish::daemon::Lock::acquire(&paths.lock),
            Ok(None)
        );
        println!(
            "daemon     {}",
            if running { "running" } else { "not running" }
        );
        println!("  socket   {}", paths.socket.display());
        println!("  log      {}", paths.log.display());
    }
    Ok(())
}
