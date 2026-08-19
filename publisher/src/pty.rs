//! Hosting a shell in a pseudo-terminal, on Windows and on Unix.
//!
//! `portable-pty` gives one interface over ConPTY and `forkpty`, which is what makes
//! a single binary cover PowerShell, cmd and a WSL shell. Its API is blocking, so the
//! moving parts here are two OS threads — one draining the PTY, one filling it — and
//! channels across to the async side. Threads rather than `spawn_blocking`: these two
//! live for the whole session, and a blocking-pool slot held forever is a leak.

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("could not open a pseudo-terminal: {0}")]
    Open(String),
    // Not named `source`: thiserror treats that as a nested error, and this is text.
    #[error("could not start {command}: {reason}")]
    Spawn { command: String, reason: String },
}

/// How much output is read from the PTY at once. Also the largest chunk handed to the
/// relay before framing, so it stays well under any negotiated frame limit.
const READ_CHUNK: usize = 16 * 1024;

/// Bounded so a terminal producing faster than the relay accepts applies back
/// pressure to the reader thread — and thence to the shell — rather than growing
/// without limit (spec §6.1 requires the publisher to respect its own bounds).
const OUTPUT_QUEUE_CHUNKS: usize = 256;
const INPUT_QUEUE_CHUNKS: usize = 256;

pub struct Pty {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    input: mpsc::Sender<Vec<u8>>,
    child_exit: tokio::sync::watch::Receiver<Option<u32>>,
}

pub struct PtyOutput {
    pub chunks: mpsc::Receiver<Vec<u8>>,
}

/// Starts `command` under a new pseudo-terminal of the given size.
pub fn spawn(command: CommandBuilder, cols: u16, rows: u16) -> Result<(Pty, PtyOutput), PtyError> {
    let described = format!("{:?}", command.get_argv());
    let system = native_pty_system();
    let pair = system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| PtyError::Open(e.to_string()))?;

    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|e| PtyError::Spawn {
            command: described,
            reason: e.to_string(),
        })?;
    // Closing the slave in this process matters: while any handle to it remains open,
    // reading the master never reaches end-of-file, so the session would hang after
    // the shell exits instead of finishing.
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| PtyError::Open(e.to_string()))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| PtyError::Open(e.to_string()))?;

    let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>(OUTPUT_QUEUE_CHUNKS);
    std::thread::Builder::new()
        .name("pty-read".into())
        .spawn(move || {
            let mut buffer = vec![0u8; READ_CHUNK];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        // blocking_send applies back pressure to the shell, which is
                        // the point: dropping terminal output would corrupt the
                        // mirror's byte stream, and offsets would never line up again.
                        if output_tx.blocking_send(buffer[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .map_err(|e| PtyError::Open(e.to_string()))?;

    let (input_tx, mut input_rx) = mpsc::channel::<Vec<u8>>(INPUT_QUEUE_CHUNKS);
    std::thread::Builder::new()
        .name("pty-write".into())
        .spawn(move || {
            while let Some(bytes) = input_rx.blocking_recv() {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
                if writer.flush().is_err() {
                    break;
                }
            }
        })
        .map_err(|e| PtyError::Open(e.to_string()))?;

    let (exit_tx, exit_rx) = tokio::sync::watch::channel(None);
    std::thread::Builder::new()
        .name("pty-wait".into())
        .spawn(move || {
            let status = child.wait().ok().map(|s| s.exit_code()).unwrap_or(0);
            let _ = exit_tx.send(Some(status));
        })
        .map_err(|e| PtyError::Open(e.to_string()))?;

    Ok((
        Pty {
            master: Arc::new(Mutex::new(pair.master)),
            input: input_tx,
            child_exit: exit_rx,
        },
        PtyOutput { chunks: output_rx },
    ))
}

impl Pty {
    /// Writes bytes to the terminal. Returns false once the shell has gone.
    pub async fn write(&self, bytes: Vec<u8>) -> bool {
        self.input.send(bytes).await.is_ok()
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Ok(master) = self.master.lock() {
            // A failed resize is not worth failing the session over: the shell keeps
            // running at its old size, and the next resize may well succeed.
            let _ = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    /// Resolves when the hosted process exits, with its exit code.
    pub async fn wait(&self) -> u32 {
        let mut exit = self.child_exit.clone();
        loop {
            if let Some(code) = *exit.borrow() {
                return code;
            }
            if exit.changed().await.is_err() {
                return 0;
            }
        }
    }
}

/// The shell to host when the user names none.
pub fn default_shell() -> CommandBuilder {
    if cfg!(windows) {
        // ComSpec is set on every Windows install; powershell.exe is not guaranteed
        // to be on PATH in a stripped image, and cmd can always start one.
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        CommandBuilder::new(shell)
    } else {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut command = CommandBuilder::new(shell);
        // A login shell, so the mirrored session has the profile the user expects.
        command.arg("-l");
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_hosted_command_produces_output_and_then_ends() {
        let mut command = if cfg!(windows) {
            let mut c = CommandBuilder::new("cmd.exe");
            c.args(["/C", "echo hypeterm"]);
            c
        } else {
            let mut c = CommandBuilder::new("/bin/sh");
            c.args(["-c", "echo hypeterm"]);
            c
        };
        command.env("TERM", "xterm-256color");

        let (pty, mut output) = spawn(command, 80, 24).expect("opens a pty");
        let mut seen = Vec::new();
        // The channel closes when the reader thread sees end-of-file, which only
        // happens because the slave handle was dropped above.
        while let Some(chunk) = output.chunks.recv().await {
            seen.extend_from_slice(&chunk);
        }
        assert!(
            String::from_utf8_lossy(&seen).contains("hypeterm"),
            "expected the command's output, got {:?}",
            String::from_utf8_lossy(&seen)
        );
        pty.wait().await;
    }

    #[tokio::test]
    async fn control_c_interrupts_the_foreground_program() {
        // 0x03 is not a byte the program reads: the *line discipline* turns it into
        // SIGINT for the terminal's foreground process group. That only happens when the
        // slave is the session's controlling terminal, so this is really a test that the
        // pty is wired up as a terminal and not merely as a pipe.
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "trap 'echo INTERRUPTED; exit 0' INT; sleep 30"]);
        command.env("TERM", "dumb");

        let (pty, mut output) = spawn(command, 80, 24).expect("opens a pty");
        // Let the shell install its trap before the signal arrives.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(pty.write(vec![0x03]).await);

        let mut seen = Vec::new();
        let collected = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(chunk) = output.chunks.recv().await {
                seen.extend_from_slice(&chunk);
                if String::from_utf8_lossy(&seen).contains("INTERRUPTED") {
                    return true;
                }
            }
            false
        })
        .await;
        assert_eq!(
            collected.ok(),
            Some(true),
            "expected ^C to raise SIGINT in the hosted program, got {:?}",
            String::from_utf8_lossy(&seen)
        );
    }

    #[tokio::test]
    async fn input_reaches_the_hosted_shell() {
        let mut command = if cfg!(windows) {
            CommandBuilder::new("cmd.exe")
        } else {
            let mut c = CommandBuilder::new("/bin/sh");
            c.arg("-i");
            c
        };
        command.env("TERM", "dumb");
        command.env("PS1", "");

        let (pty, mut output) = spawn(command, 80, 24).expect("opens a pty");
        assert!(pty.write(b"echo round-trip\r\nexit\r\n".to_vec()).await);

        let mut seen = Vec::new();
        while let Some(chunk) = output.chunks.recv().await {
            seen.extend_from_slice(&chunk);
        }
        assert!(
            String::from_utf8_lossy(&seen).contains("round-trip"),
            "expected the typed command to run, got {:?}",
            String::from_utf8_lossy(&seen)
        );
    }
}
