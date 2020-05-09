use crate::{Error as DirError, ValidatorDir};
use bls::get_withdrawal_credentials;
use deposit_contract::{encode_eth1_tx_data, Error as DepositError};
use eth2_keystore::{Error as KeystoreError, Keystore, KeystoreBuilder, PlainText};
use rand::distributions::Alphanumeric;
use rand::Rng;
use std::fs::{create_dir_all, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use types::{ChainSpec, DepositData, Hash256, Keypair, Signature};

/// The `Alphanumeric` crate only generates a-Z, A-Z, 0-9, therefore it has a range of 62
/// characters.
///
/// 62**48 is greater than 255**32, therefore this password has more bits of entropy than a byte
/// array of length 32.
const DEFAULT_PASSWORD_LEN: usize = 48;

pub const VOTING_KEYSTORE_FILE: &str = "voting-keystore.json";
pub const WITHDRAWAL_KEYSTORE_FILE: &str = "withdrawal-keystore.json";
const ETH1_DEPOSIT_DATA_FILE: &str = "eth1_deposit_data.rlp";

pub enum Error {
    DirectoryAlreadyExists(PathBuf),
    UnableToCreateDir(io::Error),
    //
    UnableToEncodeDeposit(DepositError),
    DepositDataAlreadyExists(PathBuf),
    UnableToSaveDepositData(io::Error),
    //
    KeystoreAlreadyExists(PathBuf),
    UnableToSaveKeystore(io::Error),
    //
    PasswordAlreadyExists(PathBuf),
    UnableToSavePassword(io::Error),
    //
    KeystoreError(KeystoreError),
    //
    UnableToOpenDir(DirError),
}

impl From<KeystoreError> for Error {
    fn from(e: KeystoreError) -> Error {
        Error::KeystoreError(e)
    }
}

pub struct Builder<'a> {
    dir: PathBuf,
    password_dir: PathBuf,
    voting_keystore: Option<(Keystore, PlainText)>,
    withdrawal_keystore: Option<(Keystore, PlainText)>,
    store_withdrawal_keystore: bool,
    deposit_info: Option<(u64, &'a ChainSpec)>,
}

impl<'a> Builder<'a> {
    pub fn new(dir: PathBuf, password_dir: PathBuf) -> Result<Self, Error> {
        if dir.exists() {
            Err(Error::DirectoryAlreadyExists(dir))
        } else {
            Ok(Self {
                dir,
                password_dir,
                voting_keystore: None,
                withdrawal_keystore: None,
                store_withdrawal_keystore: true,
                deposit_info: None,
            })
        }
    }

    pub fn voting_keystore(mut self, keystore: Keystore, password: &[u8]) -> Self {
        self.voting_keystore = Some((keystore, password.to_vec().into()));
        self
    }

    pub fn withdrawal_keystore(mut self, keystore: Keystore, password: &[u8]) -> Self {
        self.withdrawal_keystore = Some((keystore, password.to_vec().into()));
        self
    }

    pub fn create_eth1_tx_data(mut self, deposit_amount: u64, spec: &'a ChainSpec) -> Self {
        self.deposit_info = Some((deposit_amount, spec));
        self
    }

    pub fn build(self) -> Result<ValidatorDir, Error> {
        // Attempts to get `self.$keystore`, unwrapping it into a random keystore if it is `None`.
        // Then, decrypts the keypair from the keystore.
        macro_rules! expand_keystore {
            ($keystore: ident) => {
                self.$keystore
                    .map(Result::Ok)
                    .unwrap_or_else(random_keystore)
                    .and_then(|(keystore, password)| {
                        keystore
                            .decrypt_keypair(password.as_bytes())
                            .map(|keypair| (keystore, password, keypair))
                            .map_err(Into::into)
                    })?;
            };
        }

        let (voting_keystore, voting_password, voting_keypair) = expand_keystore!(voting_keystore);
        let (withdrawal_keystore, withdrawal_password, withdrawal_keypair) =
            expand_keystore!(withdrawal_keystore);

        if self.dir.exists() {
            return Err(Error::DirectoryAlreadyExists(self.dir));
        } else {
            create_dir_all(&self.dir).map_err(Error::UnableToCreateDir)?;
        }

        if let Some((amount, spec)) = self.deposit_info {
            let withdrawal_credentials = Hash256::from_slice(&get_withdrawal_credentials(
                &withdrawal_keypair.pk,
                spec.bls_withdrawal_prefix_byte,
            ));

            let mut deposit_data = DepositData {
                pubkey: voting_keypair.pk.clone().into(),
                withdrawal_credentials,
                amount,
                signature: Signature::empty_signature().into(),
            };

            deposit_data.signature = deposit_data.create_signature(&voting_keypair.sk, &spec);

            let deposit_data =
                encode_eth1_tx_data(&deposit_data).map_err(Error::UnableToEncodeDeposit)?;

            let path = self.dir.clone().join(ETH1_DEPOSIT_DATA_FILE);

            if path.exists() {
                return Err(Error::DepositDataAlreadyExists(path));
            } else {
                OpenOptions::new()
                    .write(true)
                    .read(true)
                    .create(true)
                    .open(path.clone())
                    .map_err(Error::UnableToSaveDepositData)?
                    .write_all(&deposit_data)
                    .map_err(Error::UnableToSaveDepositData)?
            }
        }

        write_password_to_file(
            self.password_dir
                .clone()
                .join(voting_keypair.pk.as_hex_string()),
            voting_password.as_bytes(),
        )?;

        write_keystore_to_file(
            self.dir.clone().join(VOTING_KEYSTORE_FILE),
            &voting_keystore,
        )?;

        if self.store_withdrawal_keystore {
            write_password_to_file(
                self.password_dir
                    .clone()
                    .join(withdrawal_keypair.pk.as_hex_string()),
                withdrawal_password.as_bytes(),
            )?;
            write_keystore_to_file(
                self.dir.clone().join(WITHDRAWAL_KEYSTORE_FILE),
                &withdrawal_keystore,
            )?;
        }

        ValidatorDir::open(self.dir).map_err(Error::UnableToOpenDir)
    }
}

fn write_keystore_to_file(path: PathBuf, keystore: &Keystore) -> Result<(), Error> {
    if path.exists() {
        Err(Error::KeystoreAlreadyExists(path))
    } else {
        let file = OpenOptions::new()
            .write(true)
            .read(true)
            .create(true)
            .open(path.clone())
            .map_err(Error::UnableToSaveKeystore)?;

        keystore.to_json_writer(file).map_err(Into::into)
    }
}

fn write_password_to_file(path: PathBuf, password: &[u8]) -> Result<(), Error> {
    if path.exists() {
        Err(Error::PasswordAlreadyExists(path))
    } else {
        OpenOptions::new()
            .write(true)
            .read(true)
            .create(true)
            .open(path.clone())
            .map_err(Error::UnableToSavePassword)?
            .write_all(&password)
            .map_err(Error::UnableToSavePassword)
    }
}

fn random_keystore() -> Result<(Keystore, PlainText), Error> {
    let keypair = Keypair::random();
    let password: PlainText = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(DEFAULT_PASSWORD_LEN)
        .collect::<String>()
        .into_bytes()
        .into();

    let keystore = KeystoreBuilder::new(&keypair, password.as_bytes(), "".into())?.build()?;

    Ok((keystore, password))
}
