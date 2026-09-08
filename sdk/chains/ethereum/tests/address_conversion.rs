use base::{Address as BaseAddress, Addresser};
use chain_ethereum::{Address, AddressParseError};

#[test]
fn external_address_conversion_preserves_all_twenty_bytes() {
    let bytes = std::array::from_fn(|index| index as u8);
    let source = BaseAddress::from(bytes);
    let converted = Address::try_from(&source).expect("twenty bytes form an Ethereum address");

    assert_eq!(converted, Address(bytes));
    assert_eq!(converted.address(), source);
    assert_eq!(
        converted.to_string(),
        "0x000102030405060708090a0b0c0d0e0f10111213"
    );
    assert_eq!(
        Address::try_from(&BaseAddress::from([0; 20])),
        Ok(Address([0; 20]))
    );
}

#[test]
fn external_address_conversion_rejects_every_non_twenty_byte_width() {
    for length in [0, 1, 19, 21, 32] {
        let error = Address::try_from(&BaseAddress::new(vec![0; length]))
            .expect_err("address conversion must enforce exactly twenty bytes");
        assert_eq!(error, AddressParseError::InvalidLength);
        assert_eq!(
            error.to_string(),
            "Ethereum address must contain exactly 20 bytes"
        );
    }
}
