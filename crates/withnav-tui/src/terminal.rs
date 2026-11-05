//! Owns terminal mode lifecycle and restoration; no application state transitions.
use std::io::{self, IsTerminal, Stdout};
use std::sync::Once;

use crossterm::{
    cursor::Show,
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Frame, Terminal, backend::CrosstermBackend};

use super::TuiError;

/// Raw mode, alternate screen and bracketed paste for as long as it lives.
/// Dropping it restores the terminal, also while unwinding from a panic.
pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    /// Takes over the terminal. Refuses to touch it unless stdin and stdout
    /// are both a terminal, so a pipe is never put in raw mode.
    pub fn enter() -> Result<Self, TuiError> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(TuiError::Other(
                "the terminal client needs an interactive terminal on stdin and stdout".into(),
            ));
        }
        install_panic_hook();
        match Self::setup() {
            Ok(terminal) => Ok(Self { terminal }),
            Err(err) => {
                restore();
                Err(TuiError::Other(format!(
                    "cannot set up the terminal: {err}"
                )))
            }
        }
    }

    fn setup() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        setup_modes(&mut out)?;
        Terminal::new(CrosstermBackend::new(out))
    }

    /// Draws one frame.
    pub fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> io::Result<()> {
        self.terminal.draw(render).map(|_| ())
    }

    /// Forgets the screen contents so the next draw repaints everything.
    ///
    /// Not `Terminal::clear`: it asks the terminal for the cursor position
    /// first, and a terminal busy resizing its window may not answer within
    /// crossterm's 2 s limit, which turned a window move into a fatal error.
    /// `resize` clears the same fullscreen viewport without that query.
    pub fn invalidate(&mut self) -> io::Result<()> {
        let area = self.terminal.size()?;
        self.terminal.resize(area.into())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Undoes every mode change, ignoring errors: it runs on the way out and in
/// the panic hook, where nothing can be done about a failure.
fn restore() {
    let _ = disable_raw_mode();
    let _ = restore_modes(&mut io::stdout());
}

fn setup_modes(out: &mut impl io::Write) -> io::Result<()> {
    execute!(
        out,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )
}

fn restore_modes(out: &mut impl io::Write) -> io::Result<()> {
    execute!(
        out,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    )
}

/// Restores the terminal before the previous hook prints its message, so the
/// message lands on the normal screen and stays readable.
fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mouse_modes_are_balanced() {
        let mut start = Vec::new();
        let mut end = Vec::new();
        setup_modes(&mut start).unwrap();
        restore_modes(&mut end).unwrap();
        #[cfg(unix)]
        {
            assert!(String::from_utf8(start).unwrap().contains("\x1b[?1006h"));
            assert!(String::from_utf8(end).unwrap().contains("\x1b[?1006l"));
        }
    }
}
