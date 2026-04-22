use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use std::io::{Read, Write};

pub struct PtyMaster {
    master: Box<dyn MasterPty + Send>,
}

impl PtyMaster {
    pub fn new(cmd: &[String]) -> Result<(Self, Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open pty failed")?;

        let cmd_builder = if cmd.is_empty() {
            let mut builder = CommandBuilder::new("bash");
            builder.arg("-l");
            builder
        } else {
            let mut builder = CommandBuilder::new(&cmd[0]);
            for arg in &cmd[1..] {
                builder.arg(arg);
            }
            builder
        };

        let _child = pair.slave.spawn_command(cmd_builder)?;
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let master = pair.master;

        Ok((PtyMaster { master }, reader, writer))
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("pty resize failed")
    }
}
