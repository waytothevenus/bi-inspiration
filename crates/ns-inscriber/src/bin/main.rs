use anyhow::anyhow;
use bitcoin::{address::NetworkChecked, Address, Amount, Network, PublicKey, ScriptBuf, Txid};
use chrono::{Local, SecondsFormat};
use clap::{Parser, Subcommand};
use std::{
    path::{Path, PathBuf},
    str::FromStr,
};
// use sys_locale::get_locale;
use coset::{CborSerializable, CoseEncrypt0, CoseKey, Label, TaggedCborSerializable};
use terminal_prompt::Terminal;

use ns_inscriber::{
    bitcoin::BitCoinRPCOptions,
    inscriber::{Inscriber, InscriberOptions, UnspentTxOut, UnspentTxOutJSON},
    wallet::{
        base64_decode, base64url_decode, base64url_encode, decode_sign1,
        ed25519::{self, Ed25519Key},
        encode_sign1, hash_256, iana, new_sym, secp256k1, skip_tag, with_tag, DerivationPath,
        Encrypt0, KeyHelper, CBOR_TAG,
    },
};
use ns_protocol::ns::{
    valid_name, Bytes32, Name, Operation, PublicKeyParams, Service, ThresholdLevel, Value,
};

const INS_AAD: &[u8; 12] = b"ns-inscriber";
const NS_TRANS_KEY_AAD: &[u8; 20] = b"NS:COSE/Transfer.Key";
const NS_SIGN_MESSAGE_AAD: &[u8; 19] = b"NS:COSE/Sign.Mesage";

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Run with a env config file
    #[arg(short, long, value_name = "FILE", default_value = ".env")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Generate a new KEK used to protect other keys
    NewKEK {
        /// alias for the new KEK key
        #[arg(short, long)]
        alias: String,
    },
    /// Generate a seed secret key that protected by KEK
    NewSeed {},
    /// Import a seed secret key and protected it by KEK
    ImportSeed {
        /// alias for the imported Seed key
        #[arg(short, long)]
        alias: String,
    },
    /// Export the seed key and protected it by a password
    ExportSeed {
        /// The seed key file name, will be combined to "{key}.cose.key" to read and export
        #[arg(long, value_name = "FILE")]
        seed: String,
    },
    /// Derive a Secp256k1 key from the seed, it is protected by KEK.
    /// Options Will be combined to "m/44'/0'/{acc}'/1/{idx}" (BIP-32/44)
    Secp256k1Derive {
        /// The seed key file name, will be combined to "{key}.cose.key" to read as seed key to derive
        #[arg(long, value_name = "FILE", default_value = "seed")]
        seed: String,
        #[arg(long, default_value_t = 0)]
        acc: u32,
        #[arg(long, default_value_t = 0)]
        idx: u32,
    },
    /// Derive a Ed25519 key from the seed, it is protected by KEK.
    /// Options Will be combined to "m/42'/0'/{acc}'/1/{idx}" (BIP-32/44)
    Ed25519Derive {
        /// The seed key file name, will be combined to "{key}.cose.key" to read as seed key to derive
        #[arg(long, value_name = "FILE", default_value = "seed")]
        seed: String,
        #[arg(long, default_value_t = 0)]
        acc: u32,
        #[arg(long, default_value_t = 0)]
        idx: u32,
    },
    /// List keys in keys dir.
    ListKeys {
        #[arg(short, long, default_value_t = false)]
        detail: bool,
    },
    /// Display secp256k1 addresses
    Secp256k1Address {
        /// secp256k key file name, will be combined to "{key}.cose.key" to read secp256k key
        #[arg(long, value_name = "FILE")]
        key: String,
    },
    SignMessage {
        /// Ed25519 or Secp256k1 key file name, will be combined to "{key}.cose.key" to read key
        #[arg(long, value_name = "FILE")]
        key: String,
        /// message to sign
        #[arg(long)]
        msg: String,
        /// output encoding format, default is base64, can be "hex"
        #[arg(long, default_value = "base64")]
        enc: String,
    },
    VerifyMessage {
        /// Ed25519 or Secp256k1 key file name, will be combined to "{key}.cose.key" to read key
        #[arg(long, value_name = "FILE")]
        key: String,
        /// message to verify
        #[arg(long)]
        msg: String,
        /// signature to verify
        #[arg(long, default_value = "")]
        sig: String,
        /// signature encoding format, default is base64, can be "hex"
        #[arg(long, default_value = "base64")]
        enc: String,
    },
    /// Send sats from tx to a bitcoin address
    SendSats {
        /// Unspent transaction id to spend
        #[arg(long)]
        txid: String,
        /// Bitcoin address on tx to spend, will be combined to "{addr}.cose.key" to read secp256k key
        #[arg(long, value_name = "FILE")]
        addr: String,
        /// Bitcoin address to receive sats
        #[arg(long)]
        to: String,
        /// Amount of satoshis to send
        #[arg(long)]
        amount: u64,
        /// fee rate in sat/vbyte
        #[arg(long)]
        fee: u64,
    },
    /// Preview inscription transactions and min cost
    Preview {
        /// fee rate in sat/vbyte
        #[arg(long)]
        fee: u64,
        /// ed25519 key file name, will be combined to "{key}.cose.key" to read ed25519 key
        #[arg(long, value_name = "FILE")]
        key: String,
        /// names to inscribe, separated by comma
        #[arg(long)]
        names: String,
    },
    /// Inscribe names to a transaction
    Inscribe {
        /// Unspent transaction id to spend
        #[arg(long, default_value = "")]
        txid: String,
        /// Unspent txout in JSON format. If not provided, will use the first txout on txid
        #[arg(long, default_value = "")]
        txout_json: String,
        /// Bitcoin address on tx to operate on, will be combined to "{addr}.cose.key" to read secp256k key
        #[arg(long, value_name = "FILE")]
        addr: String,
        /// fee rate in sat/vbyte
        #[arg(long)]
        fee: u64,
        /// ed25519 key file name, will be combined to "{key}.cose.key" to read ed25519 key
        #[arg(long, value_name = "FILE")]
        key: String,
        /// names to inscribe, separated by comma
        #[arg(long)]
        names: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // let locale = get_locale().unwrap_or_else(|| String::from("en-US"));
    // println!("The current locale is {}", locale);

    let cli = Cli::parse();

    let config_path = cli.config.as_deref().expect("config path not found");
    dotenvy::from_filename(config_path).expect(".env file not found");

    let keys_path = std::env::var("INSCRIBER_KEYS_DIR").unwrap_or("./keys".to_string());
    let keys_path = Path::new(&keys_path);
    if std::fs::read_dir(keys_path).is_err() {
        std::fs::create_dir_all(keys_path)?;
    }

    let network = Network::from_core_arg(&std::env::var("BITCOIN_NETWORK").unwrap_or_default())
        .unwrap_or(Network::Regtest);

    match &cli.command {
        Some(Commands::NewKEK { alias }) => {
            let mut terminal = Terminal::open()?;
            let password = terminal.prompt_sensitive("Enter a password to protect KEK: ")?;
            let mkek = hash_256(password.as_bytes());
            let kid = if alias.is_empty() {
                Value::Text(Local::now().to_rfc3339_opts(SecondsFormat::Secs, true))
            } else {
                Value::Text(alias.to_owned())
            };
            let kid = Some(kid);
            let encryptor = Encrypt0::new(mkek, None);
            let kek = new_sym(iana::Algorithm::A256GCM, kid.clone())?;
            let data = encryptor.encrypt(&kek.to_slice()?, INS_AAD, kid)?;
            let data = with_tag(&CBOR_TAG, &data);
            println!(
                "Put this new KEK as INSCRIBER_KEK on config file:\n{}",
                base64url_encode(&data)
            );
            return Ok(());
        }

        Some(Commands::NewSeed {}) => {
            let file = keys_path.join("seed.cose.key");
            if KekEncryptor::key_exists(&file) {
                println!("{} exists, skipping key generation", file.display());
                return Ok(());
            }

            let kek = KekEncryptor::open()?;
            let mut key = ed25519::Ed25519Key::new(None)?;
            let pk = key.get_public()?;
            let kid = Value::Bytes(pk.to_vec());
            key.0.set_kid(kid.clone())?;
            kek.save_key(&file, key.0, Some(Value::Bytes(pk.to_vec())))?;
            println!(
                "New seed key: {}, key id: {:?}",
                file.display(),
                hex::encode(pk)
            );
            return Ok(());
        }

        Some(Commands::ImportSeed { alias }) => {
            let alias = if alias.is_empty() {
                Local::now().to_rfc3339_opts(SecondsFormat::Secs, true) + ".seed"
            } else {
                alias.to_owned()
            };
            let file = keys_path.join(format!("{alias}.cose.key"));
            if KekEncryptor::key_exists(&file) {
                println!("{} exists, skipping key generation", file.display());
                return Ok(());
            }

            let key = {
                let mut terminal = Terminal::open()?;
                let import_key =
                    terminal.prompt("Enter the seed key (base64url encoded) to import: ")?;
                let password =
                    terminal.prompt_sensitive("Enter the password that protected the seed: ")?;
                let kek = hash_256(password.as_bytes());
                let decryptor = Encrypt0::new(kek, None);
                let ciphertext = base64url_decode(import_key.trim())?;
                let key = decryptor.decrypt(skip_tag(&CBOR_TAG, &ciphertext), NS_TRANS_KEY_AAD)?;
                Ed25519Key::from_slice(&key)?
            };

            let kek = KekEncryptor::open()?;
            let kid = key.0.kid();
            kek.save_key(&file, key.0, kid.clone())?;
            println!("Imported seed key: {}, key id: {:?}", file.display(), kid);
            return Ok(());
        }

        Some(Commands::ExportSeed { seed }) => {
            let key = {
                let kek = KekEncryptor::open()?;
                kek.read_key(&keys_path.join(format!("{seed}.cose.key")))?
            };
            let key = Ed25519Key(key);
            let kid = key.0.kid();
            let mut terminal = Terminal::open()?;
            let password = terminal.prompt_sensitive("Enter a password to protect the seed: ")?;
            let kek = hash_256(password.as_bytes());
            let encryptor = Encrypt0::new(kek, None);
            let data = encryptor.encrypt(&key.to_slice()?, NS_TRANS_KEY_AAD, kid)?;
            let data = base64url_encode(&data);

            println!("The exported seed key (base64url encoded):\n\n{data}\n\n");
            return Ok(());
        }

        Some(Commands::Secp256k1Derive { seed, acc, idx }) => {
            let kek = KekEncryptor::open()?;
            let seed_key = kek.read_key(&keys_path.join(format!("{seed}.cose.key")))?;
            let seed_key = Ed25519Key(seed_key);
            let kid = format!("m/44'/0'/{acc}'/1/{idx}");
            let path: DerivationPath = kid.parse()?;
            let secp = secp256k1::Secp256k1::new();
            let keypair =
                secp256k1::derive_secp256k1(&secp, network, &seed_key.get_secret()?, &path)?;
            let address = Address::p2wpkh(
                &PublicKey {
                    compressed: true,
                    inner: keypair.public_key(),
                },
                network,
            )?;
            let kid = Some(Value::Text(kid));
            let key =
                secp256k1::Secp256k1Key::from_secret(keypair.secret_key().as_ref(), kid.clone())?;
            let file = keys_path.join(format!("{}.cose.key", address));
            kek.save_key(&file, key.0, kid)?;
            println!("key: {}, address: {}", file.display(), address);
            return Ok(());
        }

        Some(Commands::Ed25519Derive { seed, acc, idx }) => {
            let kek = KekEncryptor::open()?;
            let seed_key = kek.read_key(&keys_path.join(format!("{seed}.cose.key")))?;
            let seed_key = Ed25519Key(seed_key);
            let kid = format!("m/42'/0'/{acc}'/1/{idx}");
            let path: DerivationPath = kid.parse()?;
            let signing_key = ed25519::derive_ed25519(&seed_key.get_secret()?, &path);
            let pk = signing_key.verifying_key().to_bytes();
            let address = format!("0x{}", hex::encode(pk));
            let kid = Value::Text(kid);
            let key = Ed25519Key::from_secret(signing_key.as_bytes(), Some(kid.clone()))?;
            let file = keys_path.join(format!("{}.cose.key", address));
            kek.save_key(&file, key.0, Some(kid))?;
            println!("key: {}, public key: {}", file.display(), address);
            return Ok(());
        }

        Some(Commands::ListKeys { detail }) => {
            let kek = if *detail {
                Some(KekEncryptor::open()?)
            } else {
                None
            };
            for entry in std::fs::read_dir(keys_path)? {
                let path = entry?.path();
                if path.is_file() {
                    let filename = &path
                        .file_name()
                        .expect("should get file name")
                        .to_string_lossy();
                    println!("\nkey file: {}", filename);

                    match kek {
                        Some(ref kek) => {
                            let key = kek.read_key(&path)?;
                            println!("key id: {}", key.kid_string());
                        }
                        None => {
                            let data = std::fs::read(&path)?;
                            let data = skip_tag(&CBOR_TAG, &data);
                            let e0 = CoseEncrypt0::from_tagged_slice(data)
                                .map_err(anyhow::Error::msg)?;
                            let cid = e0
                                .unprotected
                                .rest
                                .iter()
                                .find(|&v| v.0 == Label::Text("cid".to_string()))
                                .map(|v| &v.1);
                            if let Some(cid) = cid {
                                println!("key id: {:?}", cid);
                            }
                        }
                    }
                }
            }

            return Ok(());
        }

        Some(Commands::Secp256k1Address { key }) => {
            let kek = KekEncryptor::open()?;
            let secp256k1_key = kek.read_key(&keys_path.join(format!("{key}.cose.key")))?;
            if !secp256k1_key.is_crv(iana::EllipticCurve::Secp256k1) {
                anyhow::bail!("{} is not a secp256k1 key", key);
            }
            let secp = secp256k1::Secp256k1::new();
            let keypair =
                secp256k1::Keypair::from_seckey_slice(&secp, &secp256k1_key.get_secret()?)?;
            let (public_key, _parity) = keypair.x_only_public_key();
            let script_pubkey = ScriptBuf::new_p2tr(&secp, public_key, None);
            let address: Address<NetworkChecked> =
                Address::from_script(&script_pubkey, network).unwrap();
            println!("p2tr address: {}", address);

            let address = Address::p2pkh(
                &PublicKey {
                    compressed: true,
                    inner: keypair.public_key(),
                },
                network,
            );
            println!("p2pkh address: {}", address);

            let address = Address::p2wpkh(
                &PublicKey {
                    compressed: true,
                    inner: keypair.public_key(),
                },
                network,
            )?;
            println!("p2wpkh address: {}", address);
            return Ok(());
        }

        Some(Commands::SignMessage { key, msg, enc }) => {
            let kek = KekEncryptor::open()?;
            let cose_key = kek.read_key(&keys_path.join(format!("{key}.cose.key")))?;
            println!("message: {}", msg);

            if cose_key.is_crv(iana::EllipticCurve::Secp256k1) {
                let secp = secp256k1::Secp256k1::new();
                let keypair =
                    secp256k1::Keypair::from_seckey_slice(&secp, &cose_key.get_secret()?)?;

                let sig = secp256k1::sign_message(&secp, &keypair.secret_key(), msg);
                if enc == "hex" {
                    println!("signature:\n{}", hex::encode(sig.serialize()));
                } else {
                    println!("signature:\n{}", base64url_encode(&sig.serialize()));
                }
            } else if cose_key.is_crv(iana::EllipticCurve::Ed25519) {
                let key = ed25519::Ed25519Key(cose_key);
                let signer = key.signer()?;
                let output = encode_sign1(
                    signer,
                    msg.as_bytes().to_vec(),
                    NS_SIGN_MESSAGE_AAD.as_slice(),
                )?;
                if enc == "hex" {
                    println!("signed message:\n{}", hex::encode(&output));
                } else {
                    println!("signed message:\n{}", base64url_encode(&output));
                }
            } else {
                println!("unsupported key type");
            }
            return Ok(());
        }

        Some(Commands::VerifyMessage { key, msg, sig, enc }) => {
            let kek = KekEncryptor::open()?;
            let cose_key = kek.read_key(&keys_path.join(format!("{key}.cose.key")))?;

            if cose_key.is_crv(iana::EllipticCurve::Secp256k1) {
                let sig = if enc == "hex" {
                    hex::decode(sig)?
                } else {
                    base64_decode(sig)?
                };
                let secp = secp256k1::Secp256k1::new();
                let keypair =
                    secp256k1::Keypair::from_seckey_slice(&secp, &cose_key.get_secret()?)?;
                secp256k1::verify_message(&secp, &keypair.public_key(), msg, &sig)?;

                println!("signature is valid");
            } else if cose_key.is_crv(iana::EllipticCurve::Ed25519) {
                let msg = if enc == "hex" {
                    hex::decode(msg)?
                } else {
                    base64_decode(msg)?
                };
                let key = ed25519::Ed25519Key(cose_key);
                let verifier = key.verifier()?;
                decode_sign1(verifier, &msg, NS_SIGN_MESSAGE_AAD.as_slice())?;
                println!("signature is valid");
            } else {
                println!("unsupported key type");
            }
            return Ok(());
        }

        Some(Commands::SendSats {
            txid,
            addr,
            to,
            amount,
            fee,
        }) => {
            println!("Bitcoin network: {}", network);

            let txid: Txid = txid.parse()?;
            let to = Address::from_str(to)?.require_network(network)?;
            let amount = Amount::from_sat(*amount);
            let fee_rate = Amount::from_sat(*fee);

            let kek = KekEncryptor::open()?;
            let secp256k1_key = kek.read_key(&keys_path.join(format!("{addr}.cose.key")))?;
            if !secp256k1_key.is_crv(iana::EllipticCurve::Secp256k1) {
                anyhow::bail!("{} is not a secp256k1 key", addr);
            }
            let secp = secp256k1::Secp256k1::new();
            let keypair =
                secp256k1::Keypair::from_seckey_slice(&secp, &secp256k1_key.get_secret()?)?;
            let (p2wpkh_pubkey, p2tr_pubkey) = secp256k1::as_script_pubkey(&secp, &keypair);

            let inscriber = get_inscriber(network).await?;
            let tx = inscriber.bitcoin.get_transaction(&txid).await?;
            let (vout, txout) = tx
                .output
                .iter()
                .enumerate()
                .find(|(_, o)| {
                    o.value > amount
                        && (o.script_pubkey == p2wpkh_pubkey || o.script_pubkey == p2tr_pubkey)
                })
                .ok_or(anyhow!("no matched transaction out to spend"))?;
            let txid = inscriber
                .send_sats(
                    fee_rate,
                    &keypair.secret_key(),
                    &UnspentTxOut {
                        txid,
                        vout: vout as u32,
                        amount: txout.value,
                        script_pubkey: txout.script_pubkey.clone(),
                    },
                    &to,
                    amount,
                )
                .await?;

            println!("txid: {}", txid);
            return Ok(());
        }

        Some(Commands::Preview { fee, key, names }) => {
            println!("Bitcoin network: {}", network);

            let fee_rate = Amount::from_sat(*fee);
            let names: Vec<String> = names.split(',').map(|n| n.trim().to_string()).collect();
            for name in &names {
                if !valid_name(name) {
                    return Err(anyhow!("invalid name to inscribe: {}", name));
                }
            }
            if names.is_empty() {
                return Err(anyhow!("no names to inscribe"));
            }

            let kek = KekEncryptor::open()?;
            let ed25519_key = kek.read_key(&keys_path.join(format!("{key}.cose.key")))?;
            if !ed25519_key.is_crv(iana::EllipticCurve::Ed25519) {
                anyhow::bail!("{} is not a ed25519 key", key);
            }
            let signing_key = ed25519::SigningKey::from_bytes(&ed25519_key.get_secret()?);
            let params = PublicKeyParams {
                public_keys: vec![Bytes32(signing_key.verifying_key().to_bytes().to_owned())],
                threshold: None,
                kind: None,
            };

            let signers = vec![signing_key];
            let mut ns: Vec<Name> = Vec::with_capacity(names.len());
            for name in &names {
                let mut n = Name {
                    name: name.clone(),
                    sequence: 0,
                    service: Service {
                        code: 0,
                        operations: vec![Operation {
                            subcode: 1,
                            params: Value::from(&params),
                        }],
                        attesters: None,
                    },
                    signatures: vec![],
                };
                n.sign(&params, ThresholdLevel::All, &signers)?;
                n.validate()?;
                ns.push(n);
            }

            let res = Inscriber::preview_inscription_transactions(&ns, fee_rate)?;

            println!("inscriptions: {}", ns.len());
            println!(
                "commit_tx: {} bytes, {} vBytes",
                res.0.total_size(),
                res.0.vsize()
            );
            println!(
                "reveal_tx: {} bytes, {} vBytes",
                res.1.total_size(),
                res.1.vsize()
            );
            println!(
                "total bytes: {}, {} vBytes",
                res.0.total_size() + res.1.total_size(),
                res.0.vsize() + res.1.vsize()
            );
            println!("estimate fee: {}", res.2 - res.1.output[0].value);
            println!("min balance (fee + min change): {}", res.2);
            return Ok(());
        }

        Some(Commands::Inscribe {
            txid,
            txout_json,
            addr,
            fee,
            key,
            names,
        }) => {
            println!("Bitcoin network: {}", network);

            let fee_rate = Amount::from_sat(*fee);
            let names: Vec<String> = names.split(',').map(|n| n.trim().to_string()).collect();
            for name in &names {
                if !valid_name(name) {
                    return Err(anyhow!("invalid name to inscribe: {}", name));
                }
            }
            if names.is_empty() {
                return Err(anyhow!("no names to inscribe"));
            }

            let kek = KekEncryptor::open()?;
            let ed25519_key = kek.read_key(&keys_path.join(format!("{key}.cose.key")))?;
            if !ed25519_key.is_crv(iana::EllipticCurve::Ed25519) {
                anyhow::bail!("{} is not a ed25519 key", key);
            }
            let signing_key = ed25519::SigningKey::from_bytes(&ed25519_key.get_secret()?);
            let params = PublicKeyParams {
                public_keys: vec![Bytes32(signing_key.verifying_key().to_bytes().to_owned())],
                threshold: None,
                kind: None,
            };

            let signers = vec![signing_key];
            let mut ns: Vec<Name> = Vec::with_capacity(names.len());
            for name in &names {
                let mut n = Name {
                    name: name.clone(),
                    sequence: 0,
                    service: Service {
                        code: 0,
                        operations: vec![Operation {
                            subcode: 1,
                            params: Value::from(&params),
                        }],
                        attesters: None,
                    },
                    signatures: vec![],
                };
                n.sign(&params, ThresholdLevel::All, &signers)?;
                n.validate()?;
                ns.push(n);
            }

            let secp256k1_key = kek.read_key(&keys_path.join(format!("{addr}.cose.key")))?;
            if !secp256k1_key.is_crv(iana::EllipticCurve::Secp256k1) {
                anyhow::bail!("{} is not a secp256k1 key", key);
            }
            let secp = secp256k1::Secp256k1::new();
            let keypair =
                secp256k1::Keypair::from_seckey_slice(&secp, &secp256k1_key.get_secret()?)?;
            let (p2wpkh_pubkey, p2tr_pubkey) = secp256k1::as_script_pubkey(&secp, &keypair);

            let inscriber = get_inscriber(network).await?;
            let unspent_txout = if !txid.is_empty() {
                let txid: Txid = txid.parse()?;
                let tx = inscriber.bitcoin.get_transaction(&txid).await?;
                let (vout, txout) = tx
                    .output
                    .iter()
                    .enumerate()
                    .find(|(_, o)| {
                        o.script_pubkey == p2wpkh_pubkey || o.script_pubkey == p2tr_pubkey
                    })
                    .ok_or(anyhow!("no matched transaction out to spend"))?;
                UnspentTxOut {
                    txid,
                    vout: vout as u32,
                    amount: txout.value,
                    script_pubkey: txout.script_pubkey.clone(),
                }
            } else {
                let txout: UnspentTxOutJSON = serde_json::from_str(txout_json)?;
                txout.to()?
            };

            let txid = inscriber
                .inscribe(&ns, fee_rate, &keypair.secret_key(), &unspent_txout)
                .await?;

            println!("txid: {}", txid);
            return Ok(());
        }

        None => {}
    }

    Ok(())
}

struct KekEncryptor {
    encryptor: Encrypt0,
}

impl KekEncryptor {
    fn open() -> anyhow::Result<Self> {
        let kek_str = std::env::var("INSCRIBER_KEK").unwrap_or_default();
        if kek_str.is_empty() {
            println!("INSCRIBER_KEK not found");
            println!("KEK is used to protect other keys");
            println!("Run `ns-inscriber new-kek --alias=mykek` to generate a new KEK`");
            return Err(anyhow::Error::msg("INSCRIBER_KEK not found"));
        }

        let mut terminal = Terminal::open()?;
        let password = terminal.prompt_sensitive("Enter the password protected KEK: ")?;
        let mkek = hash_256(password.as_bytes());
        let decryptor = Encrypt0::new(mkek, None);
        let ciphertext = base64url_decode(kek_str.trim())?;
        let key = decryptor.decrypt(skip_tag(&CBOR_TAG, &ciphertext), INS_AAD)?;
        let key = CoseKey::from_slice(&key).map_err(anyhow::Error::msg)?;
        Ok(KekEncryptor {
            encryptor: Encrypt0::new(key.get_secret()?, key.kid()),
        })
    }

    fn key_exists(file: &Path) -> bool {
        std::fs::read(file).is_ok()
    }

    fn read_key(&self, file: &Path) -> anyhow::Result<CoseKey> {
        let data = std::fs::read(file)?;
        let key = self
            .encryptor
            .decrypt(skip_tag(&CBOR_TAG, &data), INS_AAD)?;
        CoseKey::from_slice(&key).map_err(anyhow::Error::msg)
    }

    fn save_key(&self, file: &Path, key: CoseKey, cid: Option<Value>) -> anyhow::Result<()> {
        let data = key.to_vec().map_err(anyhow::Error::msg)?;
        let data = self.encryptor.encrypt(&data, INS_AAD, cid)?;
        std::fs::write(file, with_tag(&CBOR_TAG, &data))?;
        Ok(())
    }
}

async fn get_inscriber(network: Network) -> anyhow::Result<Inscriber> {
    let rpcurl = std::env::var("BITCOIN_RPC_URL").unwrap();
    let rpcuser = std::env::var("BITCOIN_RPC_USER").unwrap_or_default();
    let rpcpassword = std::env::var("BITCOIN_RPC_PASSWORD").unwrap_or_default();
    let rpctoken = std::env::var("BITCOIN_RPC_TOKEN").unwrap_or_default();

    let inscriber = Inscriber::new(&InscriberOptions {
        bitcoin: BitCoinRPCOptions {
            rpcurl,
            rpcuser,
            rpcpassword,
            rpctoken,
            network,
        },
    })?;

    inscriber.bitcoin.ping().await?;
    Ok(inscriber)
}
