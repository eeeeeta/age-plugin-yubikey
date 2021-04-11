//! Structs for handling YubiKeys.

use age_core::{
    format::{FileKey, FILE_KEY_BYTES},
    primitives::{aead_decrypt, hkdf},
};
use age_plugin::{identity, Callbacks};
use bech32::{ToBase32, Variant};
use dialoguer::Password;
use secrecy::ExposeSecret;
use std::convert::TryInto;
use std::fmt;
use std::io;
use std::thread::sleep;
use std::time::{Duration, SystemTime};
use yubikey_piv::{
    certificate::{Certificate, PublicKeyInfo},
    key::{decrypt_data, AlgorithmId, RetiredSlotId, SlotId},
    yubikey::Serial,
    MgmKey, Readers, YubiKey,
};

use crate::{
    error::Error,
    format::{RecipientLine, STANZA_KEY_LABEL},
    p256::{Recipient, TAG_BYTES},
    IDENTITY_PREFIX,
};

const ONE_SECOND: Duration = Duration::from_secs(1);
const FIFTEEN_SECONDS: Duration = Duration::from_secs(15);

pub(crate) fn wait_for_readers() -> Result<Readers, Error> {
    // Start a 15-second timer waiting for a YubiKey to be inserted (if necessary).
    let start = SystemTime::now();
    loop {
        let mut readers = Readers::open()?;
        if readers.iter()?.len() > 0 {
            break Ok(readers);
        }

        match SystemTime::now().duration_since(start) {
            Ok(end) if end >= FIFTEEN_SECONDS => return Err(Error::TimedOut),
            _ => sleep(ONE_SECOND),
        }
    }
}

pub(crate) fn open(serial: Option<Serial>) -> Result<YubiKey, Error> {
    if Readers::open()?.iter()?.len() == 0 {
        if let Some(serial) = serial {
            eprintln!("⏳ Please insert the YubiKey with serial {}.", serial);
        } else {
            eprintln!("⏳ Please insert the YubiKey.");
        }
    }
    let mut readers = wait_for_readers()?;
    let mut readers_iter = readers.iter()?;

    // --serial selects the YubiKey to use. If not provided, and more than one YubiKey is
    // connected, an error is returned.
    let yubikey = match (readers_iter.len(), serial) {
        (0, _) => unreachable!(),
        (1, None) => readers_iter.next().unwrap().open()?,
        (1, Some(serial)) => {
            let yubikey = readers_iter.next().unwrap().open()?;
            if yubikey.serial() != serial {
                return Err(Error::NoMatchingSerial(serial));
            }
            yubikey
        }
        (_, Some(serial)) => {
            let reader = readers_iter
                .find(|reader| match reader.open() {
                    Ok(yk) => yk.serial() == serial,
                    _ => false,
                })
                .ok_or(Error::NoMatchingSerial(serial))?;
            reader.open()?
        }
        (_, None) => return Err(Error::MultipleYubiKeys),
    };

    Ok(yubikey)
}

pub(crate) fn manage(yubikey: &mut YubiKey) -> Result<(), Error> {
    eprintln!();
    let pin = Password::new()
        .with_prompt(&format!(
            "Enter PIN for YubiKey with serial {} (default is 123456)",
            yubikey.serial(),
        ))
        .interact()?;
    yubikey.verify_pin(pin.as_bytes())?;

    // If the user is using the default PIN, help them to change it.
    if pin == "123456" {
        eprintln!();
        eprintln!("✨ Your key is using the default PIN. Let's change it!");
        eprintln!("✨ We'll also set the PUK equal to the PIN.");
        eprintln!();
        eprintln!("🔐 The PIN is up to 8 numbers, letters, or symbols. Not just numbers!");
        eprintln!(
            "❌ Your keys will be lost if the PIN and PUK are locked after 3 incorrect tries."
        );
        eprintln!();
        let current_puk = Password::new()
            .with_prompt("Enter current PUK (default is 12345678)")
            .interact()?;
        let new_pin = Password::new()
            .with_prompt("Choose a new PIN/PUK")
            .with_confirmation("Repeat the PIN/PUK", "PINs don't match")
            .interact()?;
        if new_pin.len() > 8 {
            return Err(Error::InvalidPinLength);
        }
        yubikey.change_puk(current_puk.as_bytes(), new_pin.as_bytes())?;
        yubikey.change_pin(pin.as_bytes(), new_pin.as_bytes())?;
    }

    if let Ok(mgm_key) = MgmKey::get_protected(yubikey) {
        yubikey.authenticate(mgm_key)?;
    } else {
        // Try to authenticate with the default management key.
        yubikey
            .authenticate(MgmKey::default())
            .map_err(|_| Error::CustomManagementKey)?;

        // Migrate to a PIN-protected management key.
        let mgm_key = MgmKey::generate()?;
        mgm_key.set_protected(yubikey)?;
    }

    Ok(())
}

/// A reference to an age key stored in a YubiKey.
#[derive(Debug)]
pub struct Stub {
    pub(crate) serial: Serial,
    pub(crate) slot: RetiredSlotId,
    pub(crate) tag: [u8; TAG_BYTES],
    identity_index: usize,
}

impl fmt::Display for Stub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            bech32::encode(
                IDENTITY_PREFIX,
                self.to_bytes().to_base32(),
                Variant::Bech32,
            )
            .expect("HRP is valid")
            .to_uppercase()
            .as_str(),
        )
    }
}

impl PartialEq for Stub {
    fn eq(&self, other: &Self) -> bool {
        self.to_bytes().eq(&other.to_bytes())
    }
}

impl Stub {
    /// Returns a key stub and recipient for this `(Serial, SlotId, PublicKey)` tuple.
    ///
    /// Does not check that the `PublicKey` matches the given `(Serial, SlotId)` tuple;
    /// this is checked at decryption time.
    pub(crate) fn new(serial: Serial, slot: RetiredSlotId, recipient: &Recipient) -> Self {
        Stub {
            serial,
            slot,
            tag: recipient.tag(),
            identity_index: 0,
        }
    }

    pub(crate) fn from_bytes(bytes: &[u8], identity_index: usize) -> Option<Self> {
        let serial = Serial::from(u32::from_le_bytes(bytes[0..4].try_into().unwrap()));
        let slot: RetiredSlotId = bytes[4].try_into().ok()?;
        Some(Stub {
            serial,
            slot,
            tag: bytes[5..9].try_into().unwrap(),
            identity_index,
        })
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(9);
        bytes.extend_from_slice(&self.serial.0.to_le_bytes());
        bytes.push(self.slot.into());
        bytes.extend_from_slice(&self.tag);
        bytes
    }

    pub(crate) fn matches(&self, line: &RecipientLine) -> bool {
        self.tag == line.tag
    }

    pub(crate) fn connect<E>(
        &self,
        callbacks: &mut dyn Callbacks<E>,
    ) -> io::Result<Result<Connection, identity::Error>> {
        let mut yubikey = match YubiKey::open_by_serial(self.serial) {
            Ok(yk) => yk,
            Err(yubikey_piv::Error::NotFound) => {
                if callbacks
                    .message(&format!(
                        "Please insert YubiKey with serial {}",
                        self.serial
                    ))?
                    .is_err()
                {
                    return Ok(Err(identity::Error::Identity {
                        index: self.identity_index,
                        message: format!("Could not find YubiKey with serial {}", self.serial),
                    }));
                }

                // Start a 15-second timer waiting for the YubiKey to be inserted
                let start = SystemTime::now();
                loop {
                    match YubiKey::open_by_serial(self.serial) {
                        Ok(yubikey) => break yubikey,
                        Err(yubikey_piv::Error::NotFound) => (),
                        Err(_) => {
                            return Ok(Err(identity::Error::Identity {
                                index: self.identity_index,
                                message: format!(
                                    "Could not open YubiKey with serial {}",
                                    self.serial
                                ),
                            }));
                        }
                    }

                    match SystemTime::now().duration_since(start) {
                        Ok(end) if end >= FIFTEEN_SECONDS => {
                            return Ok(Err(identity::Error::Identity {
                                index: self.identity_index,
                                message: format!(
                                "Timed out while waiting for YubiKey with serial {} to be inserted",
                                self.serial
                            ),
                            }))
                        }
                        _ => sleep(ONE_SECOND),
                    }
                }
            }
            Err(_) => {
                return Ok(Err(identity::Error::Identity {
                    index: self.identity_index,
                    message: format!("Could not open YubiKey with serial {}", self.serial),
                }))
            }
        };

        // Read the pubkey from the YubiKey slot and check it still matches.
        let pk = match Certificate::read(&mut yubikey, SlotId::Retired(self.slot))
            .ok()
            .and_then(|cert| match cert.subject_pki() {
                PublicKeyInfo::EcP256(pubkey) => {
                    Recipient::from_encoded(pubkey).filter(|pk| pk.tag() == self.tag)
                }
                _ => None,
            }) {
            Some(pk) => pk,
            None => {
                return Ok(Err(identity::Error::Identity {
                    index: self.identity_index,
                    message: "A YubiKey stub did not match the YubiKey".to_owned(),
                }))
            }
        };

        let pin = match callbacks.request_secret(&format!(
            "Enter PIN for YubiKey with serial {}",
            self.serial
        ))? {
            Ok(pin) => pin,
            Err(_) => {
                return Ok(Err(identity::Error::Identity {
                    index: self.identity_index,
                    message: format!("A PIN is required for YubiKey with serial {}", self.serial),
                }))
            }
        };
        if yubikey.verify_pin(pin.expose_secret().as_bytes()).is_err() {
            return Ok(Err(identity::Error::Identity {
                index: self.identity_index,
                message: "Invalid YubiKey PIN".to_owned(),
            }));
        }

        Ok(Ok(Connection {
            yubikey,
            pk,
            slot: self.slot,
            tag: self.tag,
        }))
    }
}

pub(crate) struct Connection {
    yubikey: YubiKey,
    pk: Recipient,
    slot: RetiredSlotId,
    tag: [u8; 4],
}

impl Connection {
    pub(crate) fn recipient(&self) -> &Recipient {
        &self.pk
    }

    pub(crate) fn unwrap_file_key(&mut self, line: &RecipientLine) -> Result<FileKey, ()> {
        assert_eq!(self.tag, line.tag);

        // The YubiKey API for performing scalar multiplication takes the point in its
        // uncompressed SEC-1 encoding.
        let shared_secret = match decrypt_data(
            &mut self.yubikey,
            line.epk_bytes.decompress().as_bytes(),
            AlgorithmId::EccP256,
            SlotId::Retired(self.slot),
        ) {
            Ok(res) => res,
            Err(_) => return Err(()),
        };

        let mut salt = vec![];
        salt.extend_from_slice(line.epk_bytes.as_bytes());
        salt.extend_from_slice(self.pk.to_encoded().as_bytes());

        let enc_key = hkdf(&salt, STANZA_KEY_LABEL, shared_secret.as_ref());

        // A failure to decrypt is fatal, because we assume that we won't
        // encounter 32-bit collisions on the key tag embedded in the header.
        match aead_decrypt(&enc_key, FILE_KEY_BYTES, &line.encrypted_file_key) {
            Ok(pt) => Ok(TryInto::<[u8; FILE_KEY_BYTES]>::try_into(&pt[..])
                .unwrap()
                .into()),
            Err(_) => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use yubikey_piv::{key::RetiredSlotId, Serial};

    use super::Stub;

    #[test]
    fn stub_round_trip() {
        let stub = Stub {
            serial: Serial::from(42),
            slot: RetiredSlotId::R1,
            tag: [7; 4],
            identity_index: 0,
        };

        let encoded = stub.to_bytes();
        assert_eq!(Stub::from_bytes(&encoded, 0), Some(stub));
    }
}