use anyhow::{Context, Result};
use crossterm::terminal;
use std::io::{self, IsTerminal};

pub struct TerminalState;

impl TerminalState {
    pub fn make_raw() -> Result<Self> {
        terminal::enable_raw_mode().context("enable raw mode failed")?;
        Ok(TerminalState)
    }

    pub fn restore() -> Result<()> {
        terminal::disable_raw_mode().context("disable raw mode failed")?;
        Ok(())
    }

    pub fn size() -> Result<(u16, u16)> {
        terminal::size().context("get terminal size failed")
    }
}

pub fn is_stdin_terminal() -> bool {
    io::stdin().is_terminal()
}
