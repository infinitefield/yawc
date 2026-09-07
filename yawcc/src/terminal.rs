use std::io::{self, BufRead, Write};

use anyhow::Context;
use rustyline::{error::ReadlineError, ExternalPrinter};
use tokio::sync::mpsc;
use yawc::frame::Frame;

pub enum Input {
    Line(String),
    Frame(Frame),
    Interrupted,
    Error(io::Error),
}

pub type Receiver = mpsc::Receiver<Input>;

pub struct Output {
    printer: Option<Box<dyn ExternalPrinter + Send>>,
}

impl Output {
    pub fn plain() -> Self {
        Self { printer: None }
    }

    pub fn message(&mut self, message: &str) -> anyhow::Result<()> {
        if let Some(printer) = &mut self.printer {
            printer
                .print(format!("< {message}"))
                .context("write terminal output")?;
        } else {
            let mut out = io::stdout().lock();
            writeln!(out, "{message}")?;
            out.flush()?;
        }
        Ok(())
    }

    pub fn bytes(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        let mut out = io::stdout().lock();
        out.write_all(bytes)?;
        out.flush()?;
        Ok(())
    }
}

pub fn scripted(frames: Vec<Option<Frame>>) -> (Receiver, Output) {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        for frame in frames.into_iter().flatten() {
            if tx.send(Input::Frame(frame)).await.is_err() {
                break;
            }
        }
    });
    (rx, Output::plain())
}

pub fn start(interactive: bool, history: bool) -> anyhow::Result<(Receiver, Output)> {
    let (tx, rx) = mpsc::channel(64);
    if !interactive {
        std::thread::spawn(move || {
            let stdin = io::stdin();
            for line in stdin.lock().lines() {
                let event = match line {
                    Ok(line) => Input::Line(line),
                    Err(err) => Input::Error(err),
                };
                let failed = matches!(event, Input::Error(_));
                if tx.blocking_send(event).is_err() || failed {
                    break;
                }
            }
        });
        return Ok((rx, Output::plain()));
    }
    let mut editor = rustyline::DefaultEditor::with_config(
        rustyline::Config::builder().max_history_size(1000)?.build(),
    )?;
    let history_path = history
        .then(home::home_dir)
        .flatten()
        .map(|p| p.join(".yawcc_history"));
    if let Some(path) = &history_path {
        if !path.exists() {
            // Read the old spelling once so existing history is preserved.
            let _ = editor.load_history(&path.with_file_name(".yawc_history"));
        } else {
            let _ = editor.load_history(path);
        }
    }
    let printer = editor.create_external_printer()?;
    std::thread::spawn(move || loop {
        if tx.is_closed() {
            break;
        }
        let event = match editor.readline("> ") {
            Ok(line) => {
                let _ = editor.add_history_entry(&line);
                if let Some(path) = &history_path {
                    if let Err(err) = editor.save_history(path) {
                        eprintln!("warning: could not save history: {err}");
                    }
                }
                Input::Line(line)
            }
            Err(ReadlineError::Interrupted) => Input::Interrupted,
            Err(ReadlineError::Eof) => break,
            Err(err) => Input::Error(io::Error::other(err.to_string())),
        };
        let stop = matches!(event, Input::Interrupted | Input::Error(_));
        if tx.blocking_send(event).is_err() || stop {
            break;
        }
    });
    Ok((
        rx,
        Output {
            printer: Some(Box::new(printer)),
        },
    ))
}

// Rustyline restores modes when readline returns. A remote disconnect can end
// the process while readline is blocked, so main also owns an original snapshot.
#[cfg(unix)]
pub struct Restore(Option<nix::sys::termios::Termios>);

#[cfg(unix)]
impl Restore {
    pub fn capture() -> Self {
        Self(nix::sys::termios::tcgetattr(io::stdin()).ok())
    }
}

#[cfg(unix)]
impl Drop for Restore {
    fn drop(&mut self) {
        use nix::sys::termios::{tcsetattr, SetArg};
        if let Some(mode) = &self.0 {
            let changed =
                nix::sys::termios::tcgetattr(io::stdin()).is_ok_and(|current| current != *mode);
            let _ = tcsetattr(io::stdin(), SetArg::TCSANOW, mode);
            if changed && std::io::IsTerminal::is_terminal(&io::stdout()) {
                // readline may also have left bracketed paste or a hidden cursor on.
                let _ = io::stdout().write_all(b"\x1b[?2004l\x1b[?25h\r\n");
                let _ = io::stdout().flush();
            }
        }
    }
}

#[cfg(windows)]
pub struct Restore(Vec<(windows_sys::Win32::Foundation::HANDLE, u32)>);

#[cfg(windows)]
impl Restore {
    pub fn capture() -> Self {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        let mut modes = Vec::new();
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE] {
            // These handles are borrowed from the process, and GetConsoleMode
            // rejects redirected handles without dereferencing application memory.
            unsafe {
                let handle = GetStdHandle(which);
                let mut mode = 0;
                if GetConsoleMode(handle, &mut mode) != 0 {
                    modes.push((handle, mode));
                }
            }
        }
        Self(modes)
    }
}

#[cfg(windows)]
impl Drop for Restore {
    fn drop(&mut self) {
        for &(handle, mode) in &self.0 {
            // Restore only handles successfully captured above; none are closed here.
            unsafe {
                windows_sys::Win32::System::Console::SetConsoleMode(handle, mode);
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
pub struct Restore;
#[cfg(not(any(unix, windows)))]
impl Restore {
    pub fn capture() -> Self {
        Self
    }
}
