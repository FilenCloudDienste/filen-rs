//! Known-answer tests: every key derivation, cipher and RSA encoding the client relies on, pinned
//! to bytes produced by the code as it stands, so that a dependency upgrade which changes what is
//! written or accepted fails here instead of on existing accounts' data.
//!
//! The RSA keys were generated with rsa 0.9's keygen. Key (b) has a private exponent one byte
//! shorter than its modulus, which pins the unpadded big-endian bytes `HMACKey::new` feeds HKDF.
//! The V1 metadata strings were made with
//! `openssl enc -aes-256-cbc -md md5 -salt -pass pass:<key> -base64 -A`.

use std::{borrow::Cow, str::FromStr};

use base64::{Engine, prelude::BASE64_STANDARD};
use filen_types::{
	api::v3::dir::link::info::LinkPasswordSalt,
	crypto::{EncryptedString, rsa::RSAEncryptedString},
	serde::str::SizedHexString,
};
use rsa::{
	Oaep, RsaPrivateKey, RsaPublicKey,
	pkcs1::EncodeRsaPublicKey,
	pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey},
	traits::PrivateKeyParts,
};
use sha2::{Sha256, Sha512};

use crate::{
	auth::v2::hash_name,
	crypto::{
		connect::derive_password_for_link,
		rsa::{HMACKey, blocking_decrypt_with_private_key, blocking_encrypt_with_public_key},
		shared::{DataCrypter, MetaCrypter},
		v1,
		v2::{self, MasterKey, MasterKeys},
		v3::{self, EncryptionKey},
	},
};

const FIRST_KEY: &str = "0123456789abcdefghijklmnopqrstuv";
const SECOND_KEY: &str = "vutsrqponmlkjihgfedcba9876543210";
const META_JSON: &str = r#"{"name":"a","size":1}"#;

/// `META_JSON` in the OpenSSL EVP layout (CryptoJS output), under `FIRST_KEY` and `SECOND_KEY`.
const V1_META_FIRST_KEY: &str = "U2FsdGVkX183cOTZuIowDTPZ3cx+uOTl/DWwl81T7fTdSwIjY3H2Pik27LM+VABu";
const V1_META_SECOND_KEY: &str = "U2FsdGVkX185OyOdQx1zelsJXH8h5zzjQ8LDekkeB1ivQvYxIAyYMZsTI3bPsqqM";

/// `META_JSON` encrypted by `MasterKey(FIRST_KEY)` and by `MasterKey(SECOND_KEY)`.
const V2_META_FIRST_KEY: &str =
	"002TmvWyoZdGWJvWFhhl4Y7PjyUaGQIgvVTxP1PtRhaPp0zQ19H6RnKEOa9fdYepQ==";
const V2_META_SECOND_KEY: &str =
	"002fILDGLTUvAkEKeTvBhn+nAQoondrmbpTfpfE9gS8dOiiUeSzym2Aw+jnV2qxbg==";

/// `META_JSON` encrypted by `EncryptionKey::new([0x42; 32])`.
const V3_META: &str =
	"0030a691da0e0ae2fe460d41753tPH5+bfJtbh01Rv6BoWxuOVuTX1Ks703pYf6Ifr9mXQ+mSEY4w==";

/// The bytes `0..len` encrypted as file data, `(len, hex)`: by `v2::FileKey(FIRST_KEY)` ...
const V2_DATA: [(u8, &str); 3] = [
	(
		1,
		"7a5fcda118490d9ce16c1f95128e599b24d3967f007ac651e51f66b3fc",
	),
	(
		17,
		"7f6536bf9fe24cd42cf9b7dccf3e4120b8384ddde1209389ca8277a0f5b26086d7cbfb75658f4e7f4a5310e6e0",
	),
	(
		33,
		"2b94fc2d2f9e0928bf07481af040d01eb0fc1b6fcb60467518b7fd8a7cb6e790c20e4842fdc188b743d94b0022fddaf29bdae055faff8d40e98fe3b8bd",
	),
];

/// ... and by `EncryptionKey::new([0x11; 32])`.
const V3_DATA: [(u8, &str); 3] = [
	(
		1,
		"4ffa6ce821250a375d06841d13b9991591279e27b2f4d3d51f7d6bc1de",
	),
	(
		17,
		"b0f32e110263cb967de927d97ee8cc4b739759e435151d73964ec09c98917eb234a7b3624da7c6e892329af3f9",
	),
	(
		33,
		"418393fb2554c3d92d2e35ff140c357f7dcdee24fc2800dc1e0da7eecab824925ce5bd9f9573e508cc6aea564e411c676caae995e273e892cb019ca7cf",
	),
];

/// Key (a): a 2048-bit key whose private exponent takes the full 256 bytes.
const KEY_A_PKCS8: &str = "MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQC+D4A6GKYAvhges+72uoRkLYxRxPu8pZdaUYbJ3Z21eEXqt54m96d0KI+LGH+kNcz6AVYeLB9zIXwO4qX3niG9fZNkIDf6KmCtTTYFXvKd1P/KPDPLtJ183WpC3Y/p8t5FDgUpKcQ2i4WsMC7EbTlnTaLXnUxlYCttM0VJ8PCWJy+rX6Spi/kPbwzu0fuXAHSqHpJcIuWgTmbXjzRTd6VWQPDCWprHIWIODW69pach8Hcnd0de5ySrZbfg2bKXcdG+9j3KZcUWaL4P8sXQ/IBH5jyyuS9ZzYqABHxutC7PNpfDCPLQ8ndTx8Y56YiE5jMdhmX6nlAqYo7G+hYhXNYrAgMBAAECggEAJmHVAGaH57B8CH9H6/XA1MZKBx1jxTYfubKrKMsjlUlDvWH1g2MSIygpzOpWpJ/k1Dw+2vsIIuUJO6xT3gzGPZFFmbvT8iZqvbTRCwagsnUYbwv3XdUM1PGWCwjZjmJotsNdEXII2QcWKK7WUv7Hz+/iye5PRk3El1n6TTNmMrw/96ZMVtA+99sK+byD75pESLwMQXOUTzCAnp40LkujWjSk+JA6gwVe6m008LE5W0SJIVRwoMRYnaC5GNL0keMk/LVJ209qnJDTm6YwCxiDOGgaufsBgp+Xk2NIb5Tz5kZkafhXT2OIzQ4aH31KhHICW1Pq9iNFW8mX5sLmyBy4wQKBgQDSTg5popGXnUOmEmECLfskhi58QyEL0benEFTLjxzd/uwMzSFqnnVkZ+0pt0uyru70ucRRZQ+q81bH4lPez5aoeImMWq4FLjp7eMS4y/wM+BEE7t34XpcECzxX/AByp8nFwms4waEOuVLP4rMRY4QAFobIfL8WXmhqzgzoP3ivoQKBgQDnW2C+XJi+QAOKNFMne25YiPo42MYCX30B/F2ny4jDUiXYgMbaLQupKgVvLKWLhnKFWhMYOk+uwcztI+k9qlhAqchO7pmsXwhnN2Ey4xBh9op5qfE8jyTDysgvOXqgS9KCuq2GOgyGl6dxcfmYCZAGoSKA9c+vG9z6Uh6H+u4iSwKBgCOnliv0ouhogjqarRW4g/aqRSFOJEEjuhY535ZvWf6NJKn68SppWQ+/yuaiq9PMcrjM8eq2zpQ2E0VqKrwAx9MiNHe2PACm2MVsMTX/Z2kveXszoDBmoQXPfdiQ4eaOadf8YT9LHKopY0/ZaHCPRabaKtm8xIzfTqak7m1FrYlhAoGBAKltV8nQkeMxieYaL5NIZyTbGzOqPEfcmfVsVUHeYgd4D1Mf64A3YMS/iz9BWPJ2wUM7FaS2f0ghkOtRUyvfpcWlEJYNUejTp5m/CqVzd7nmDL7QX2PUCD6CMFu1IkJlhIJMJ2uW8zlVsj/4EarvxdO1CUmbuj4oj4mZvdYjQ2VFAoGBAIdgxXah7nHlmx3jDivsstc1I826PzIJnTCz1l5jAxHnxhV5DbqH3WAAS8v2WZbeRntNxIdR0aexpcCKyW5Dv2PjgICf0UKoBUjXUGrEU8wWgK6QVTK5YPgNAOnJSeQPaD5rAdYRjlLBRXabwQqIUaDZdgn1dZTB8XlgheQ123Vn";
const KEY_A_SPKI: &str = "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAvg+AOhimAL4YHrPu9rqEZC2MUcT7vKWXWlGGyd2dtXhF6reeJvendCiPixh/pDXM+gFWHiwfcyF8DuKl954hvX2TZCA3+ipgrU02BV7yndT/yjwzy7SdfN1qQt2P6fLeRQ4FKSnENouFrDAuxG05Z02i151MZWArbTNFSfDwlicvq1+kqYv5D28M7tH7lwB0qh6SXCLloE5m1480U3elVkDwwlqaxyFiDg1uvaWnIfB3J3dHXuckq2W34Nmyl3HRvvY9ymXFFmi+D/LF0PyAR+Y8srkvWc2KgAR8brQuzzaXwwjy0PJ3U8fGOemIhOYzHYZl+p5QKmKOxvoWIVzWKwIDAQAB";
const KEY_A_PKCS1_PUBLIC: &str = "MIIBCgKCAQEAvg+AOhimAL4YHrPu9rqEZC2MUcT7vKWXWlGGyd2dtXhF6reeJvendCiPixh/pDXM+gFWHiwfcyF8DuKl954hvX2TZCA3+ipgrU02BV7yndT/yjwzy7SdfN1qQt2P6fLeRQ4FKSnENouFrDAuxG05Z02i151MZWArbTNFSfDwlicvq1+kqYv5D28M7tH7lwB0qh6SXCLloE5m1480U3elVkDwwlqaxyFiDg1uvaWnIfB3J3dHXuckq2W34Nmyl3HRvvY9ymXFFmi+D/LF0PyAR+Y8srkvWc2KgAR8brQuzzaXwwjy0PJ3U8fGOemIhOYzHYZl+p5QKmKOxvoWIVzWKwIDAQAB";
const KEY_A_HMAC_DOCS: &str = "e51a14ad887ba63d08dd47ae82bb2e6d0f8c400123aa3c788f53d508d1a6957e";
const KEY_A_HMAC_EMPTY: &str = "9d0e62f5720cdd2de5f5c1c34de0310243ac85a1eb337fbdc26276d5be269a23";

/// Key (b): a 2048-bit key whose private exponent is 255 bytes long.
const KEY_B_PKCS8: &str = "MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDndL6jKH/GUO//kmSgjCjFE+VVpznPJe7QRpDIXcB/htJXcElNKD3Y5PdmBMc3+N01clLq+uJUj/DL4Q8Oo6gVTx27N6E4Sy+o6UsekcQSpJCD7i8YS2LpPj/vqxF8DUXwjeDq4V4+2U+7JQ+kaIuTRP9g/ycpRUjDcIRQurlE0VA7Z72KenAFNBwVH1kZDcRw8wYn86oFO4l7xDtgvwV47dYutnrelfNgamnALyqP3hVprBxq5kplbflMFtElhNnjt/rhTCt9+WKJD5M0Sm/zPIvZgOi2lBKoOqlxLfDdAgYTgLE8umjDLbA2GFJAoESnkENiihibjB0OvBSQJJPdAgMBAAECggEAANKobtoRAjp9b/IsS2nUJ0c31IMgsHzZCYKGtz+mFI39J3Rtvkl7Vs8RlhpEPw2yc6MxakZ0EYrld0QYkpvAWDsMxwKenx2dUMNrkfNAuDQ/k0iAgFycQGoJPifkiwVdD0rh1tPwStbo8qhiEtqQTHi9U7H0iLAHXzKCNetEB1uZaz3kCT/fCth3xN+eZUnMnCVB097dgYwHFs6GizD0jDsYV5fYMIxjM3kDAuEJ/UxtU5IqQ6bWPUeEc4TmZ28HaXjYZ1/nycoKwKfkMKs4bUlmU2FP1JFplNGRx8zBwsW+W6jtNQD3PiO1utPf4vd+U1p9HpcuRSwCWWf/GIc8oQKBgQD4NkgviafSRrvPCWIfhrYiGgpfiphRsR8viTD2ouLufsCxJqiTMDbyX6JbF6PBKBL0lOqdljPoFCEZq8zOugHuQuFzaiwjQ4WLTr/9RYrGO4Z237ii5OoB/aW5ib0gBrdnMYPlgs8A6j90IZVb5CgJ+33t8xShUDKJjMUnUirViQKBgQDut9+GKHpmBVNeQA0imdGz+sNS+OMr7VgU6Rw494sSYL++GFR2FQQWCD0WeCVgPq75R5ShF5tuRpSV+OYcbCJnO3lGZk3/xiztiLXvaVp56nFJ9K/erAqWdJRHZTPFRLUNaND+V/G6IpitwXGyalXZDg+d3QAOh3jdw4HpXpJKtQKBgQDIIqSjx//MWgJXFr7Kosk6x/pzPQJCZGm734feZkpKeWOPs5cbTzYOyjPT9USxm0llcqmWNSVXTNu/ojn6uhqSbdTxRbufF3OXdwDxC+CwpBBwy0VJ+wAAaO/2oWwgZan+B/vM1GNq3nYZgna26Emset74PRb2Vv7Yz+usH+Gv2QKBgQCd34C2KuL3waHm/RkyvuOO0dKHdw5WKLtN6PS/QAb7enyUbWa+SRvQrlYoHAEr64iwV4SWHRSwE4GExXz5YxJFj3Ge+12uQBPKa9VWpigt5LxTmr0M2F6QLj06ElXwqoXKfn1pGCRHY0Xx87MLuYNY7xGo51V4RmPK1dd+zQqW2QKBgEim7nH29Nge09t4hlAbGOly54mA9PH9VWRkjRFoMzknfJ94jOCDiTZiiKZUWKI4JKWQJWUjSkTEVe9cqPMDvdmC4tlfBli77nvtbot33Zqr58kMsXSn1nobz9VlRhoOUWQH7bFENfVChf1ae46HsSv+nzr3usz2MgAeME+W5l/g";
const KEY_B_SPKI: &str = "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA53S+oyh/xlDv/5JkoIwoxRPlVac5zyXu0EaQyF3Af4bSV3BJTSg92OT3ZgTHN/jdNXJS6vriVI/wy+EPDqOoFU8duzehOEsvqOlLHpHEEqSQg+4vGEti6T4/76sRfA1F8I3g6uFePtlPuyUPpGiLk0T/YP8nKUVIw3CEULq5RNFQO2e9inpwBTQcFR9ZGQ3EcPMGJ/OqBTuJe8Q7YL8FeO3WLrZ63pXzYGppwC8qj94VaawcauZKZW35TBbRJYTZ47f64UwrffliiQ+TNEpv8zyL2YDotpQSqDqpcS3w3QIGE4CxPLpowy2wNhhSQKBEp5BDYooYm4wdDrwUkCST3QIDAQAB";
const KEY_B_PKCS1_PUBLIC: &str = "MIIBCgKCAQEA53S+oyh/xlDv/5JkoIwoxRPlVac5zyXu0EaQyF3Af4bSV3BJTSg92OT3ZgTHN/jdNXJS6vriVI/wy+EPDqOoFU8duzehOEsvqOlLHpHEEqSQg+4vGEti6T4/76sRfA1F8I3g6uFePtlPuyUPpGiLk0T/YP8nKUVIw3CEULq5RNFQO2e9inpwBTQcFR9ZGQ3EcPMGJ/OqBTuJe8Q7YL8FeO3WLrZ63pXzYGppwC8qj94VaawcauZKZW35TBbRJYTZ47f64UwrffliiQ+TNEpv8zyL2YDotpQSqDqpcS3w3QIGE4CxPLpowy2wNhhSQKBEp5BDYooYm4wdDrwUkCST3QIDAQAB";
const KEY_B_HMAC_DOCS: &str = "7b0e5ef7c95fccdb7ade723d7d29df9c1ace5f961b0ecfaa6260faec51faefa1";
const KEY_B_HMAC_EMPTY: &str = "eda364f07134875f588e2b5809e1928a2cf56517043f904b5e88e07d3ffa3a57";

/// OAEP-SHA512 ciphertexts under key (a): of `b"filen"` and of the 126 bytes `0..126`, the most
/// a 2048-bit key takes with SHA-512 OAEP.
const RSA_FILEN: &str = "iJKyy1jEohLX9C9dO29LC14P7Ucinfu0PKwO2uRCXWd9GxUt4quXxpq9GPVBijw1OVepeks3Yh1g3h2RxOCNvvv4ocF2lMeKQHACheSQ1grgPnw8HkSgFjlmAAkRl8y/o/xWYqM9nJmTk/KXFik4gphQzN4FruwbUxM82sGezT8W6MyX+IIRLZQqrCG2P4IsL2CDfaM62PHuBbjXpMAEfNkmxLfNbcnLA4RWr8bLmxi/QPwsKOeXQSfz48jiOuJMoEy5O7TVcxK8lYnaYyXxDyNqd/T5MAEuWgvAV2WMh31Pck9HW5IqORG1ZOUS6EzM8UfrmJY9DbfkqdAa3Kn5LA==";
const RSA_126_BYTES: &str = "JiRVW4FADBwgafPSOgFshAhcYhborUlDkK5cgdx8oYUnoXf4nCPhAH47se/iHsTGccNj2XTMReKZlrQJdWbisw3Zdtb7frIALGu6/o8WCLAnkInw3cELeGOLRtowd3yckYDZBmDUqMQ/m2fINFC3xjdA6+9X8ZoDONgOixE+55NM1ZVKGCBnOborXCiUqQfub0R8uJY1r+WKNpG195MfT7WrIjpR0KGj+bRcYM33VjD1Ww02p8+jo2RJ5CrEtNl1Kk2zrBgfTEb+4vdAmGZD9UnsgpvaC2p0ZpUYdc2/7stm33RmYXG7E1ZShDhQZuYGV5ucNLz311UbPNFX7ciXmQ==";

/// OAEP-SHA512 ciphertext of `b"filen"` under key (b), made with
/// `openssl pkeyutl -encrypt -pkeyopt rsa_padding_mode:oaep -pkeyopt rsa_oaep_md:sha512
/// -pkeyopt rsa_mgf1_md:sha512`.
const RSA_FILEN_KEY_B: &str = "qo4VD+K3OOPl5UEL37swL2z0Ij7CWZeUq74OQY6B6suIpV3np9FDCtqxmbnRtlEcnsesMWk1HsaMBUZ4dDw7FDa4ErbI8Q6gbPPj5YmPKZh4QLYJNcDKf5l6M1d/mmZ5zPRHjE/4H3fnJDUM/VsMMHVvoVDooI/zvj2G9VhdwbwOsX5qQOKq6qzM9VTOWtR/HgotYPWUwhF4xJ4QsF+lni29GoUpTrzOEYT6IhOUN8F7T1ljf6BWJ0U3wjn+f2vTTmbHWwYUxv7imKddR0K9EczDrdFbzr2PXXU3TWEfhy3/q7TG0jMK9yXIqMOPT7bj93HBdPk8OzQKqE6mhfCTkw==";

/// The length of an RSA ciphertext under a 2048-bit key: the modulus length, whatever the
/// plaintext.
const RSA_2048_CIPHERTEXT_LEN: usize = 256;

/// `KEY_A_PKCS8` as stored on the server: its base64 text encrypted by `MasterKey(FIRST_KEY)`.
const ENCRYPTED_PRIVATE_KEY_A: &str = "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";

/// SHA-1 of the hex of SHA-512 of `b"password123"`: the V1 master key and the link password hash.
const HASH_PASSWORD123: &str = "0d7ab85e1039a61d62206b2e6ec0c6a28734ead1";

/// The salt and password of `connect::tests::check_known_hashes_v3`, and its pinned 128-hex-char
/// Argon2id output.
const V3_SALT: &str = "67b9cda4866f766fbe643739198e4125481c6cd1d3d7f50070dcab4c857f5f6efcb61dcbe28475ce5c5bdb5cf976b6d6392f9128809671a997b2159ed45628d7705a302387ae13c26b8762490d79f57cbca70f2ac06d6bafab63e240b45c9db9afb105ea08692914941a5e0ddb3575d4fa40083bba1797c8c77b0e72e1dffd20cc929bd355b50ce93898ca486bd556cd60ce01fa5eedc1e4a34295c1cace184617f6ce0a8a4ccc5059cb93b4afe22615934c9f03a238530299e495c18c903d60482f45291db94053cc655514e5aa87003ad912461a88efca639c86b4abcd24efdb6939c42612997a003257c9eb6a5e7e440afbdce279f5beb9f0f731d2d6f3f3";
const V3_PASSWORD: &str = "password";
const V3_DERIVED: &str = "906c444ee01ab94e55e550f4dff2ac72648483aa9f783a95ce8b5e2308afae713ebca8d836a8186025768a5474e16678a6c2a5eee306c7450e6815f6d0caf9d7";

fn encrypted(meta: &str) -> EncryptedString<'_> {
	EncryptedString(Cow::Borrowed(meta))
}

fn master_key(key: &str) -> MasterKey {
	MasterKey::from_str(key).expect("a 32-char master key")
}

fn private_key(pkcs8_base64: &str) -> RsaPrivateKey {
	RsaPrivateKey::from_pkcs8_der(&BASE64_STANDARD.decode(pkcs8_base64).unwrap()).unwrap()
}

fn public_key(spki_base64: &str) -> RsaPublicKey {
	RsaPublicKey::from_public_key_der(&BASE64_STANDARD.decode(spki_base64).unwrap()).unwrap()
}

fn counting_bytes(len: u8) -> Vec<u8> {
	(0..len).collect()
}

fn decrypt_data(crypter: &impl DataCrypter, hex_data: &str) -> Vec<u8> {
	let mut data = hex::decode(hex_data).unwrap();
	crypter.blocking_decrypt_data(&mut data).unwrap();
	data
}

fn rsa_ciphertext(base64: &str) -> RSAEncryptedString<'_> {
	RSAEncryptedString(Cow::Borrowed(base64))
}

// V1

#[test]
fn v1_password_derivation_gives_the_pinned_master_key_and_derived_password() {
	let (master_key, derived_password) = v1::derive_password_and_mk(b"password123").unwrap();
	assert_eq!(master_key.as_ref(), HASH_PASSWORD123);
	assert_eq!(
		derived_password.0,
		"7465e95234c0f7fed7608be0039f95b3570dc56cdd825ea61bc103c35828e054e2c063ab054b3341d11efd171c68d58971f34aa630387b50c2ad2cbcdd226dbcd42138444bf07a71f21e00a72a3cf09d3f80855d3fdf447765cd31df70d3bb6a7e2c680359d0ca717681a809129f936c411b88ae114fefe86d39678bb7376e91"
	);
}

#[test]
fn the_sha512_then_sha1_hash_gives_the_pinned_digest() {
	assert_eq!(hex::encode(v2::hash(b"password123")), HASH_PASSWORD123);
}

#[test]
fn name_hashes_ignore_case_and_give_the_pinned_digest() {
	let expected = "455f3194ec552fbc452397fc4d6676427fe1bb96";
	assert_eq!(hash_name("Docs").to_string(), expected);
	assert_eq!(hash_name("docs").to_string(), expected);
}

#[test]
fn a_link_password_without_a_salt_hashes_to_the_pinned_digest() {
	assert_eq!(
		derive_password_for_link(Some("password"), &LinkPasswordSalt::None)
			.unwrap()
			.0,
		"4e253244a9e480cc2224913923a626ef28d0a5b9"
	);
}

#[test]
fn a_v1_file_key_decrypts_openssl_salted_metadata() {
	let key = v1::FileKey::from_str(FIRST_KEY).unwrap();
	assert_eq!(
		key.blocking_decrypt_meta(&encrypted(V1_META_FIRST_KEY))
			.unwrap(),
		META_JSON
	);
}

#[test]
fn a_master_key_falls_back_to_v1_for_openssl_salted_metadata() {
	assert_eq!(
		master_key(FIRST_KEY)
			.blocking_decrypt_meta(&encrypted(V1_META_FIRST_KEY))
			.unwrap(),
		META_JSON
	);
}

#[test]
fn master_keys_try_each_key_in_turn_for_v1_metadata() {
	let keys = MasterKeys(vec![master_key(FIRST_KEY), master_key(SECOND_KEY)]);
	assert_eq!(
		keys.decrypt_meta_into_v1(&encrypted(V1_META_SECOND_KEY), Vec::new())
			.unwrap(),
		META_JSON
	);
}

// V2

#[test]
fn a_master_key_decrypts_its_pinned_v2_metadata() {
	assert_eq!(
		master_key(FIRST_KEY)
			.blocking_decrypt_meta(&encrypted(V2_META_FIRST_KEY))
			.unwrap(),
		META_JSON
	);
}

#[test]
fn a_master_key_rejects_metadata_of_another_key() {
	assert!(
		master_key(FIRST_KEY)
			.blocking_decrypt_meta(&encrypted(V2_META_SECOND_KEY))
			.is_err()
	);
}

#[test]
fn master_keys_decrypt_v2_metadata_with_a_later_key() {
	let keys = MasterKeys(vec![master_key(FIRST_KEY), master_key(SECOND_KEY)]);
	assert_eq!(
		keys.blocking_decrypt_meta(&encrypted(V2_META_SECOND_KEY))
			.unwrap(),
		META_JSON
	);
}

#[test]
fn a_v2_file_key_decrypts_its_pinned_data() {
	let key = v2::FileKey::from_str(FIRST_KEY).unwrap();
	for (len, ciphertext) in V2_DATA {
		assert_eq!(decrypt_data(&key, ciphertext), counting_bytes(len), "{len}");
	}
}

#[test]
fn v2_password_derivation_gives_the_pinned_master_key_and_derived_password() {
	let (master_key, derived_password) =
		v2::derive_password_and_mk(b"password", b"filen-fixed-salt-0123456789abcdef").unwrap();
	assert_eq!(
		master_key.as_ref(),
		"c387243a52f7a94cbb5850b90b89acd56dd0a4bbd4c0fd14f10c9b3e06a07dac"
	);
	assert_eq!(
		derived_password.0,
		"ae2319242cf549bc15e965d9142cc66ce9981a0914971fb5726a27158ef4ffb6185589dd665824853c140d757eeff197d2c589df8a16787c40f876c5a08f7709"
	);
}

#[test]
fn a_master_key_decrypts_the_pinned_encrypted_private_key() {
	let decrypted = master_key(FIRST_KEY)
		.blocking_decrypt_meta(&encrypted(ENCRYPTED_PRIVATE_KEY_A))
		.unwrap();
	assert_eq!(decrypted, KEY_A_PKCS8);
}

// V3

#[test]
fn an_encryption_key_decrypts_its_pinned_v3_metadata() {
	assert_eq!(
		EncryptionKey::new([0x42; 32])
			.blocking_decrypt_meta(&encrypted(V3_META))
			.unwrap(),
		META_JSON
	);
}

#[test]
fn an_encryption_key_decrypts_its_pinned_data() {
	let key = EncryptionKey::new([0x11; 32]);
	for (len, ciphertext) in V3_DATA {
		assert_eq!(decrypt_data(&key, ciphertext), counting_bytes(len), "{len}");
	}
}

#[test]
fn v3_password_derivation_splits_the_argon2_output_into_kek_and_derived_password() {
	let salt = SizedHexString::new_from_hex_str(V3_SALT).unwrap();
	let (kek, derived_password) =
		v3::derive_password_and_kek(V3_PASSWORD.as_bytes(), &salt).unwrap();
	let (kek_hex, derived_password_hex) = V3_DERIVED.split_at(64);
	assert_eq!(kek.to_string(), kek_hex);
	assert_eq!(derived_password.0, derived_password_hex);
}

// RSA

#[test]
fn private_keys_reencode_to_their_pinned_pkcs8() {
	for pkcs8 in [KEY_A_PKCS8, KEY_B_PKCS8] {
		let reencoded = private_key(pkcs8).to_pkcs8_der().unwrap();
		assert_eq!(BASE64_STANDARD.encode(reencoded.as_bytes()), pkcs8);
	}
}

#[test]
fn private_keys_match_their_pinned_spki_public_keys() {
	for (pkcs8, spki) in [(KEY_A_PKCS8, KEY_A_SPKI), (KEY_B_PKCS8, KEY_B_SPKI)] {
		assert_eq!(*private_key(pkcs8).as_ref(), public_key(spki));
	}
}

#[test]
fn public_keys_encode_to_their_pinned_pkcs1() {
	for (spki, pkcs1) in [
		(KEY_A_SPKI, KEY_A_PKCS1_PUBLIC),
		(KEY_B_SPKI, KEY_B_PKCS1_PUBLIC),
	] {
		let encoded = public_key(spki).to_pkcs1_der().unwrap();
		assert_eq!(BASE64_STANDARD.encode(encoded.as_bytes()), pkcs1);
	}
}

#[test]
fn key_b_has_a_private_exponent_shorter_than_its_modulus() {
	assert_eq!(private_key(KEY_A_PKCS8).d().to_bytes_be().len(), 256);
	assert_eq!(private_key(KEY_B_PKCS8).d().to_bytes_be().len(), 255);
}

#[test]
fn hmac_keys_derived_from_private_keys_give_the_pinned_hashes() {
	for (pkcs8, docs, empty) in [
		(KEY_A_PKCS8, KEY_A_HMAC_DOCS, KEY_A_HMAC_EMPTY),
		(KEY_B_PKCS8, KEY_B_HMAC_DOCS, KEY_B_HMAC_EMPTY),
	] {
		let hmac = HMACKey::new(&private_key(pkcs8));
		assert_eq!(hmac.hash_to_string(b"docs").to_string(), docs);
		assert_eq!(hex::encode(<[u8; 32]>::from(hmac.hash(b""))), empty);
	}
}

#[test]
fn a_private_key_decrypts_the_pinned_oaep_sha512_ciphertexts() {
	let key = private_key(KEY_A_PKCS8);
	assert_eq!(
		blocking_decrypt_with_private_key(&key, &rsa_ciphertext(RSA_FILEN)).unwrap(),
		b"filen"
	);
	assert_eq!(
		blocking_decrypt_with_private_key(&key, &rsa_ciphertext(RSA_126_BYTES)).unwrap(),
		counting_bytes(126)
	);
}

#[test]
fn the_pinned_ciphertext_fails_with_sha256_as_label_or_mgf1_hash() {
	let key = private_key(KEY_A_PKCS8);
	let ciphertext = BASE64_STANDARD.decode(RSA_FILEN).unwrap();
	assert!(key.decrypt(Oaep::new::<Sha256>(), &ciphertext).is_err());
	assert!(
		key.decrypt(Oaep::new_with_mgf_hash::<Sha256, Sha512>(), &ciphertext)
			.is_err()
	);
	assert!(
		key.decrypt(Oaep::new_with_mgf_hash::<Sha512, Sha256>(), &ciphertext)
			.is_err()
	);
}

#[test]
fn data_encrypted_with_a_public_key_decrypts_with_its_private_key() {
	let key = private_key(KEY_A_PKCS8);
	let encrypted = blocking_encrypt_with_public_key(&public_key(KEY_A_SPKI), b"filen").unwrap();
	assert_eq!(
		blocking_decrypt_with_private_key(&key, &encrypted).unwrap(),
		b"filen"
	);
}

#[test]
fn a_private_key_with_a_short_exponent_decrypts_an_openssl_oaep_sha512_ciphertext() {
	assert_eq!(
		blocking_decrypt_with_private_key(
			&private_key(KEY_B_PKCS8),
			&rsa_ciphertext(RSA_FILEN_KEY_B)
		)
		.unwrap(),
		b"filen"
	);
}

#[test]
fn public_key_ciphertexts_take_the_modulus_length_and_decrypt_with_the_private_key() {
	for (pkcs8, spki) in [(KEY_A_PKCS8, KEY_A_SPKI), (KEY_B_PKCS8, KEY_B_SPKI)] {
		let key = private_key(pkcs8);
		for plaintext in [Vec::new(), b"filen".to_vec(), counting_bytes(126)] {
			let encrypted =
				blocking_encrypt_with_public_key(&public_key(spki), &plaintext).unwrap();
			assert_eq!(
				BASE64_STANDARD.decode(encrypted.0.as_ref()).unwrap().len(),
				RSA_2048_CIPHERTEXT_LEN
			);
			assert_eq!(
				blocking_decrypt_with_private_key(&key, &encrypted).unwrap(),
				plaintext
			);
		}
	}
}

#[test]
fn a_2048_bit_public_key_refuses_more_than_126_bytes() {
	assert!(
		blocking_encrypt_with_public_key(&public_key(KEY_A_SPKI), &counting_bytes(127)).is_err()
	);
}
