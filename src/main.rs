use anyhow::{Context, Result};
use log::{LevelFilter, error, info};

use crate::gadget::run_gadget;

mod gadget;

fn main() -> Result<()> {
    env_logger::builder()
		.filter_level(LevelFilter::Off)
		.filter_module("carplayd", LevelFilter::Trace)
        .parse_default_env()
        .init();

	info!("starting gadget mode");
    let success = run_gadget("a6549874b44f916a6a2cd63e35d8655245623ccd").context("failed to run gadget")?;
	if !success {
		error!("iAP2 accessory rejected us");
		return Ok(());
	}

	info!("starting host mode");

    Ok(())
}
