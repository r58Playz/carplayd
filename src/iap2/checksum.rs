fn sum(bytes: &[u8]) -> u8 {
	bytes
		.iter()
		.copied()
		.fold(0u8, |accumulator, byte| accumulator.wrapping_add(byte))
}

pub fn calculate(bytes: &[u8]) -> u8 {
	0u8.wrapping_sub(sum(bytes))
}

pub fn verify(bytes: &[u8], checksum: u8) -> bool {
	calculate(bytes) == checksum
}

pub fn is_zero_sum(bytes_with_checksum: &[u8]) -> bool {
	sum(bytes_with_checksum) == 0
}

#[cfg(test)]
mod tests {
	use super::{calculate, is_zero_sum, verify};

	#[test]
	fn checksum_round_trip() {
		let payload = [0x01, 0x02, 0x03, 0xf0, 0x55];
		let checksum = calculate(&payload);

		assert!(verify(&payload, checksum));

		let mut with_checksum = payload.to_vec();
		with_checksum.push(checksum);
		assert!(is_zero_sum(&with_checksum));
	}
}
