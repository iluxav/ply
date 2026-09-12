use anyhow::Result;

use crate::cli::UiArgs;

pub fn exec(_args: UiArgs) -> Result<()> {
    crate::tui::run()
}
