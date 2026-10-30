//! Pure wallet generation and validation shared by protected enrollment and isolated
//! development fixtures. These helpers cannot authorize or activate connections.
//! Production callers must enter through verified protected enrollment.
use super::*;
use gofin_discovery::{GenerateConnectionArgs, WalletNetwork};
use zeroize::Zeroizing;

pub fn connection_args(args: GenerateConnectionArgs) -> Result<AddConnectionArgs> {
    validate_name(&args.name)?;
    let provider = match args.network {
        WalletNetwork::Evm => Provider::Evm,
        WalletNetwork::Solana => Provider::Solana,
    };
    parse(json!({"provider":provider,"name":args.name,"credential_type":"private_key"}))
}

fn generation_failed() -> GoFinError {
    error(
        "CREDENTIAL_GENERATION_FAILED",
        "The protected wallet key could not be generated.",
    )
}

pub fn generate(provider: Provider) -> Result<SecretMaterial> {
    generate_with(provider, |bytes| {
        getrandom::fill(bytes).map_err(|_| generation_failed())
    })
}

fn generate_with(
    provider: Provider,
    mut fill: impl FnMut(&mut [u8]) -> Result<()>,
) -> Result<SecretMaterial> {
    let mut secret = Zeroizing::new([0u8; 32]);
    match provider {
        Provider::Evm => loop {
            fill(secret.as_mut())?;
            let private_key = Zeroizing::new(hex::encode(secret.as_ref()));
            // The existing secp256k1 library validates the scalar, including zero/order bounds.
            if gofin_evm::LocalEnvelopeSigner::from_private_key(&private_key).is_ok() {
                return Ok(SecretMaterial::EvmPrivateKey {
                    private_key: private_key.to_string(),
                });
            }
        },
        Provider::Solana => {
            fill(secret.as_mut())?;
            let signing_key = ed25519_dalek::SigningKey::from_bytes(&secret);
            let mut keypair = Zeroizing::new([0u8; 64]);
            keypair[..32].copy_from_slice(secret.as_ref());
            keypair[32..].copy_from_slice(&signing_key.verifying_key().to_bytes());
            Ok(SecretMaterial::SolanaPrivateKey {
                private_key: bs58::encode(keypair.as_ref()).into_string(),
            })
        }
        _ => Err(generation_failed()),
    }
}

pub fn validate(
    args: &AddConnectionArgs,
    staged: Option<&CredentialRef>,
    material: Option<&SecretMaterial>,
) -> Result<(ConnectionConfig, Vec<Value>, Vec<String>)> {
    let reference = staged.ok_or_else(generation_failed)?.as_str().to_owned();
    let connection = match (args.provider, material) {
        (Provider::Evm, Some(SecretMaterial::EvmPrivateKey { private_key })) => {
            let address = format!(
                "{:#x}",
                gofin_evm::LocalEnvelopeSigner::from_private_key(private_key)?.address()
            );
            ConnectionConfig::Evm {
                name: args.name.clone(),
                resource: ResourceId(format!("evm:{address}")),
                address,
                credential_ref: Some(reference),
                signing_enabled: true,
            }
        }
        (Provider::Solana, Some(SecretMaterial::SolanaPrivateKey { private_key })) => {
            let address = gofin_solana::SolanaSigner::parse(private_key)?
                .address()
                .to_owned();
            let network = gofin_solana::NETWORK.to_owned();
            ConnectionConfig::Solana {
                name: args.name.clone(),
                resource: ResourceId(format!("{network}:{address}")),
                address,
                network,
                credential_ref: Some(reference),
                signing_enabled: true,
            }
        }
        _ => return Err(generation_failed()),
    };
    Ok((
        connection,
        vec![json!({"name":"wallet identity","status":"passed","signer_derived":true})],
        vec![],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn args(network: WalletNetwork) -> AddConnectionArgs {
        connection_args(GenerateConnectionArgs {
            network,
            name: "generated-wallet".into(),
        })
        .unwrap()
    }

    fn reference() -> CredentialRef {
        CredentialRef::new("11111111-1111-4111-8111-111111111111").unwrap()
    }

    #[test]
    fn generated_wallet_keys_use_existing_credentials_and_signer_formats() {
        for network in [WalletNetwork::Evm, WalletNetwork::Solana] {
            let args = args(network);
            let material = generate(args.provider).unwrap();
            material.validate_present().unwrap();
            // This is the existing credential-store serialization contract, not a new seed format.
            let encoded = Zeroizing::new(serde_json::to_vec(&material).unwrap());
            let reloaded: SecretMaterial = serde_json::from_slice(&encoded).unwrap();
            let reference = reference();
            let (connection, checks, warnings) =
                validate(&args, Some(&reference), Some(&reloaded)).unwrap();
            assert_eq!(
                credential(&connection).unwrap().as_str(),
                reference.as_str()
            );
            assert!(warnings.is_empty());
            assert_eq!(checks.len(), 1);
            let second = generate(args.provider).unwrap();
            let (other, _, _) = validate(&args, Some(&reference), Some(&second)).unwrap();
            assert_ne!(resource(&connection), resource(&other));
            assert!(validate(&args, None, Some(&material)).is_err());
            assert!(validate(&args, Some(&reference), None).is_err());
            let mut config = setup_configuration(501, BrokerPolicy::beta());
            config.connections.push(connection.clone());
            config.validate().unwrap();
            effective_protected_config(config.clone()).unwrap();
            assert_eq!(
                connection_target("generate", &config, "generated-wallet")
                    .unwrap_err()
                    .code,
                "CONNECTION_NAME_CONFLICT"
            );
            assert_eq!(
                connection_target("update", &config, "generated-wallet").unwrap(),
                Some(0)
            );
            assert_eq!(
                connection_target("generate", &config, "another").unwrap(),
                None
            );
            let review = presentation::review(&json!({"action":"generate","connection":connection,"checks":checks,"warnings":[],"financial_movement":false,"future_financial_authority":false})).unwrap();
            assert!(review.contains("Save generated"));
            assert!(!review.contains("private_key"));
            let result = outcome("generate", Some(&connection), checks, warnings, None);
            assert_eq!(result["connection"]["name"], "generated-wallet");
            assert_eq!(
                result["connection"]["address"],
                serde_json::to_value(&connection).unwrap()["address"]
            );
            assert!(!result.to_string().contains(reference.as_str()));
            assert!(!result.to_string().contains("private_key"));
            assert!(!result.to_string().contains("credential_ref"));
        }
    }

    #[test]
    fn evm_generation_rejects_invalid_scalars_and_entropy_failures_are_safe() {
        let calls = Cell::new(0);
        let material = generate_with(Provider::Evm, |bytes| {
            let call = calls.get();
            calls.set(call + 1);
            bytes.fill(if call == 0 {
                0
            } else if call == 1 {
                255
            } else {
                1
            });
            Ok(())
        })
        .unwrap();
        assert_eq!(calls.get(), 3);
        material.validate_present().unwrap();
        for provider in [Provider::Evm, Provider::Solana] {
            let failure = generate_with(provider, |_| Err(generation_failed())).unwrap_err();
            assert_eq!(failure.code, "CREDENTIAL_GENERATION_FAILED");
            assert!(!failure.to_string().contains("private_key"));
        }
        assert!(
            generate_with(Provider::Binance, |_| panic!(
                "unsupported provider must not consume entropy"
            ))
            .is_err()
        );
    }

    #[test]
    fn generated_request_rejects_secret_or_unknown_fields_and_invalid_names() {
        for network in ["evm", "solana"] {
            for field in [
                "private_key",
                "seed",
                "mnemonic",
                "credential_ref",
                "approval",
                "derivation_path",
            ] {
                let failure = parse::<GenerateConnectionArgs>(
                    json!({"network":network,"name":"wallet",field:"SYNTHETIC-SECRET"}),
                )
                .unwrap_err();
                assert!(!failure.to_string().contains("SYNTHETIC-SECRET"));
            }
        }
        for network in ["stellar", "hedera", "ethereum"] {
            assert!(
                parse::<GenerateConnectionArgs>(json!({"network":network,"name":"wallet"}))
                    .is_err()
            );
        }
        for name in ["", "UPPER", "two words", "../wallet"] {
            assert!(
                connection_args(GenerateConnectionArgs {
                    network: WalletNetwork::Evm,
                    name: name.into()
                })
                .is_err()
            );
        }
    }

    #[test]
    fn failed_generation_removes_only_unactivated_staging_and_retains_uncertain_keys() {
        let reference = reference();
        let empty = setup_configuration(501, BrokerPolicy::beta());
        // The shared failure path removes a staged key only after readback proves it inactive.
        let removed = Cell::new(false);
        cleanup_failed_candidate(
            &reference,
            || Ok(empty.clone()),
            |_| {
                removed.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert!(removed.get());
        let failure = cleanup_failed_candidate(
            &reference,
            || Ok(empty.clone()),
            |_| Err(generation_failed()),
        )
        .unwrap_err();
        assert_eq!(failure.code, "ENROLLMENT_CLEANUP_REQUIRED");
        let args = args(WalletNetwork::Evm);
        let material = generate(args.provider).unwrap();
        let mut activated = empty;
        activated.connections.push(
            validate(&args, Some(&reference), Some(&material))
                .unwrap()
                .0,
        );
        for observed in [Ok(activated), Err(generation_failed())] {
            let failure = cleanup_failed_candidate(
                &reference,
                || observed,
                |_| panic!("possibly active keys must be retained"),
            )
            .unwrap_err();
            assert_eq!(failure.code, "ENROLLMENT_OUTCOME_UNCERTAIN");
        }
    }

    #[tokio::test]
    async fn reloaded_generated_evm_key_signs_and_verifies_existing_envelope() {
        use gofin_evm::EnvelopeSigner;
        let material = generate(Provider::Evm).unwrap();
        let encoded = Zeroizing::new(serde_json::to_vec(&material).unwrap());
        let reloaded: SecretMaterial = serde_json::from_slice(&encoded).unwrap();
        let SecretMaterial::EvmPrivateKey { private_key } = &reloaded else {
            panic!("wrong credential type")
        };
        let signer = gofin_evm::LocalEnvelopeSigner::from_private_key(private_key).unwrap();
        let transaction: gofin_evm::FrozenTransaction = serde_json::from_value(json!({
            "chain_id":1,"sender":format!("{:#x}",signer.address()),
            "to":"0x1111111111111111111111111111111111111111","value":"0","calldata":"0x",
            "nonce":0,"gas":21000,"max_fee_per_gas":"2","max_priority_fee_per_gas":"1"
        }))
        .unwrap();
        let signed = signer.sign_prepared(&transaction).await.unwrap();
        gofin_evm::verify_signed_envelope(&transaction, &signed).unwrap();
        let mut changed = transaction.clone();
        changed.nonce = 1;
        assert!(gofin_evm::verify_signed_envelope(&changed, &signed).is_err());
    }

    struct SyntheticSigningRpc;
    #[async_trait::async_trait]
    impl gofin_solana::SolanaRpc for SyntheticSigningRpc {
        async fn request(&self, method: &str, _params: Value) -> Result<Value> {
            assert!(
                matches!(method, "getSlot" | "getBlockHeight"),
                "signing test permits only slot/height reads, never dispatch"
            );
            Ok(json!(1))
        }
    }

    struct VerifyAndStopJournal(std::sync::atomic::AtomicBool);
    impl gofin_domain::SubmissionJournal for VerifyAndStopJournal {
        fn assert_authorized(&self) -> Result<()> {
            Ok(())
        }
        fn persist_submission(&self, submission: &gofin_domain::Submission) -> Result<()> {
            use base64::Engine;
            use bincode::Options;
            let wire = base64::engine::general_purpose::STANDARD
                .decode(
                    submission.data["signed_transaction_base64"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap();
            let transaction: solana_transaction::versioned::VersionedTransaction =
                bincode::DefaultOptions::new()
                    .with_fixint_encoding()
                    .reject_trailing_bytes()
                    .deserialize(&wire)
                    .unwrap();
            transaction.verify_and_hash_message().unwrap();
            assert_eq!(
                transaction.message.static_account_keys()[0].to_string(),
                submission.data["signer"].as_str().unwrap()
            );
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            // Stop before the runtime can send even to a fake RPC endpoint.
            Err(GoFinError::validation(
                "Synthetic signing verified; dispatch intentionally stopped.",
            ))
        }
    }

    #[tokio::test]
    async fn reloaded_generated_solana_key_signs_and_verifies_existing_runtime_message() {
        let material = generate(Provider::Solana).unwrap();
        let encoded = Zeroizing::new(serde_json::to_vec(&material).unwrap());
        let reloaded: SecretMaterial = serde_json::from_slice(&encoded).unwrap();
        let SecretMaterial::SolanaPrivateKey { private_key } = &reloaded else {
            panic!("wrong credential type")
        };
        let signer = gofin_solana::SolanaSigner::parse(private_key).unwrap();
        let runtime = gofin_solana::SolanaRuntime::new(Arc::new(SyntheticSigningRpc));
        let frozen = runtime
            .freeze_instructions(
                &[gofin_solana::NormalizedInstruction {
                    program_id: "11111111111111111111111111111111".into(),
                    accounts: vec![],
                    data_base64: String::new(),
                }],
                &[],
                "11111111111111111111111111111111",
                &signer,
                100,
                5000,
                None,
                &[],
            )
            .await
            .unwrap();
        let prepared = gofin_domain::PreparedOperation {
            operation: "wallet.transfer".into(),
            input: json!({}),
            binding: json!({"message_sha256":frozen.message_sha256()}),
            summary: "Synthetic generated-key signing only".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: now(),
            expires_at: (chrono::Utc::now() + chrono::Duration::minutes(1)).to_rfc3339(),
            valuation: None,
            intent: gofin_domain::FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };
        let journal = Arc::new(VerifyAndStopJournal(std::sync::atomic::AtomicBool::new(
            false,
        )));
        let failure = runtime
            .dispatch(&frozen, &signer, &prepared, journal.clone())
            .await
            .unwrap_err();
        assert!(failure.message.contains("Synthetic signing verified"));
        assert!(journal.0.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn uninstalled_process_cannot_generate_wallets() {
        for network in ["evm", "solana"] {
            let failure = handle("generate", json!({"network":network,"name":"wallet"}))
                .await
                .unwrap_err();
            assert!(matches!(
                failure.code.as_str(),
                "PROTECTED_BROKER_REQUIRED" | "INVALID_INSTALLATION" | "AUTHORIZATION_UNAVAILABLE"
            ));
        }
    }
}
