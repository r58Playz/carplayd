use anyhow::{Context, Result, bail};

use crate::{
	carplay::net::{conn::HeadUnitTcpConnection, head_unit::HeadUnitServer},
	pairing::{
		identity::{Identity, PairingStore},
		keys::SessionKeys,
	},
};

pub mod codec;
pub mod conn;
pub mod head_unit;

pub struct CarplayNetSession {
	server: HeadUnitServer,
	shared_secret: [u8; 32],
	control: HeadUnitTcpConnection,
}
impl CarplayNetSession {
	pub async fn new(
		head_unit: HeadUnitServer,
		identity: &mut Identity,
		pairing: &PairingStore,
	) -> Result<Self> {
		let mut control = HeadUnitTcpConnection::new(head_unit.addr)
			.await
			.context("failed to connect to control socket")?;

		let ltpk = match pairing.get(&head_unit.pairing_identity) {
			Some(x) => x,
			None => {
				let res = control
					.pair_setup(identity)
					.await
					.context("failed to pair_setup")?;
				if res.accessory_id != head_unit.pairing_identity {
					bail!(
						"{:?} accessory_id did not match {:?} pairing_identity",
						res.accessory_id,
						head_unit.pairing_identity
					);
				}
				res.accessory_ltpk
			}
		};

		let shared_secret = control
			.pair_verify(identity, ltpk)
			.await
			.context("failed to pair_verify")?;

		control
			.upgrade(SessionKeys::control(&shared_secret))
			.context("failed to upgrade")?;

		control.mfi_auth().await.context("failed to do mfi auth")?;

		Ok(Self {
			server: head_unit,
			shared_secret,
			control,
		})
	}

	pub async fn run(self) -> Result<()> {
		// TODO
		Ok(())
	}
}
