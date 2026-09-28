//! `sarab settings get|put TABLE KEY [VALUE]`: Android's Settings provider,
//! over binder (IPlatform), the way `adb shell settings` would.

use anyhow::Result;
use clap::{Subcommand, ValueEnum};
use sarab_runtime::settings;

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum Table {
    System,
    Secure,
    Global,
}

impl Table {
    fn id(self) -> i32 {
        match self {
            Table::System => settings::SYSTEM,
            Table::Secure => settings::SECURE,
            Table::Global => settings::GLOBAL,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum SettingsCmd {
    #[command(about = "Print a setting")]
    Get {
        #[arg(value_enum)]
        table: Table,
        key: String,
    },
    #[command(about = "Change a setting")]
    Put {
        #[arg(value_enum)]
        table: Table,
        key: String,
        value: String,
    },
}

pub fn run(c: SettingsCmd) -> Result<()> {
    let p = crate::app::connect()?;
    match c {
        SettingsCmd::Get { table, key } => println!("{}", p.settings_get_string(table.id(), &key)?),
        SettingsCmd::Put { table, key, value } => p.settings_put_string(table.id(), &key, &value)?,
    }
    Ok(())
}
