use crate::starknet_commands::declare::Declare;
use crate::starknet_commands::{call, declare, deploy, invoke, tx_status};
use crate::{get_account, WaitForTx};
use anyhow::{anyhow, Context, Result};
use blockifier::execution::contract_class::TrackedResource;
use blockifier::execution::deprecated_syscalls::DeprecatedSyscallSelector;
use blockifier::execution::entry_point::CallEntryPoint;
use blockifier::execution::execution_utils::ReadOnlySegments;
use blockifier::execution::syscalls::hint_processor::SyscallHintProcessor;
use blockifier::state::cached_state::CachedState;
use cairo_lang_casm::hints::Hint;
use cairo_lang_casm::instructions::Instruction;
use cairo_lang_runnable_utils::builder::{
    create_code_footer, create_entry_code_from_params, BuildError, EntryCodeConfig, RunnableBuilder,
};
use cairo_lang_runner::casm_run::hint_to_hint_params;
use cairo_lang_runner::short_string::as_cairo_short_string;
use cairo_lang_runner::{Arg, RunResultValue, SierraCasmRunner};
use cairo_lang_sierra::extensions::ConcreteType;
use cairo_lang_sierra::program::{Function, VersionedProgram};
use cairo_lang_sierra_to_casm::metadata::MetadataComputationConfig;
use cairo_lang_utils::ordered_hash_map::OrderedHashMap;
use cairo_vm::serde::deserialize_program::HintParams;
use cairo_vm::types::builtin_name::BuiltinName;
use cairo_vm::types::relocatable::Relocatable;
use cairo_vm::vm::errors::hint_errors::HintError;
use cairo_vm::vm::vm_core::VirtualMachine;
use camino::Utf8PathBuf;
use clap::Args;
use conversions::byte_array::ByteArray;
use conversions::serde::deserialize::BufferReader;
use forge_runner::running::{has_segment_arena, syscall_handler_offset};
use runtime::starknet::context::{build_context, SerializableBlockInfo};
use runtime::starknet::state::DictStateReader;
use runtime::{
    CheatcodeHandlingResult, EnhancedHintError, ExtendedRuntime, ExtensionLogic, StarknetRuntime,
    SyscallHandlingResult,
};
use scarb_api::{package_matches_version_requirement, StarknetContractArtifacts};
use scarb_metadata::{Metadata, PackageMetadata};
use semver::{Comparator, Op, Version, VersionReq};
use shared::print::print_as_warning;
use shared::utils::build_readable_text;
use sncast::get_nonce;
use sncast::helpers::configuration::CastConfig;
use sncast::helpers::constants::SCRIPT_LIB_ARTIFACT_NAME;
use sncast::helpers::fee::{FeeArgs, ScriptFeeSettings};
use sncast::helpers::rpc::RpcArgs;
use sncast::response::structs::ScriptRunResponse;
use sncast::state::hashing::{
    generate_declare_tx_id, generate_deploy_tx_id, generate_invoke_tx_id,
};
use sncast::state::state_file::StateManager;
use starknet::accounts::{Account, SingleOwnerAccount};
use starknet::core::types::{BlockId, BlockTag::Pending};
use starknet::providers::jsonrpc::HttpTransport;
use starknet::providers::JsonRpcClient;
use starknet::signers::LocalWallet;
use starknet_types_core::felt::Felt;
use std::collections::HashMap;
use std::fs;
use tokio::runtime::Runtime;

type ScriptStarknetContractArtifacts = StarknetContractArtifacts;

#[derive(Args, Debug)]
#[command(about = "Execute a deployment script")]
pub struct Run {
    /// Module name that contains the `main` function, which will be executed
    pub script_name: String,

    /// Specifies scarb package to be used
    #[clap(long)]
    pub package: Option<String>,

    /// Do not use the state file
    #[clap(long)]
    pub no_state_file: bool,

    #[clap(flatten)]
    pub rpc: RpcArgs,
}

pub struct CastScriptExtension<'a> {
    pub provider: &'a JsonRpcClient<HttpTransport>,
    pub account: Option<&'a SingleOwnerAccount<&'a JsonRpcClient<HttpTransport>, LocalWallet>>,
    pub tokio_runtime: Runtime,
    pub config: &'a CastConfig,
    pub artifacts: &'a HashMap<String, StarknetContractArtifacts>,
    pub state: StateManager,
}

impl CastScriptExtension<'_> {
    pub fn account(
        &self,
    ) -> Result<&SingleOwnerAccount<&JsonRpcClient<HttpTransport>, LocalWallet>> {
        self.account.ok_or_else(|| anyhow!("Account not defined. Please ensure the correct account is passed to `script run` command"))
    }
}

impl<'a> ExtensionLogic for CastScriptExtension<'a> {
    type Runtime = StarknetRuntime<'a>;

    #[allow(clippy::too_many_lines)]
    fn handle_cheatcode(
        &mut self,
        selector: &str,
        mut input_reader: BufferReader,
        _extended_runtime: &mut Self::Runtime,
    ) -> Result<CheatcodeHandlingResult, EnhancedHintError> {
        let res = match selector {
            "call" => {
                let contract_address = input_reader.read()?;
                let function_selector = input_reader.read()?;
                let calldata_felts = input_reader.read()?;

                let call_result = self.tokio_runtime.block_on(call::call(
                    contract_address,
                    function_selector,
                    calldata_felts,
                    self.provider,
                    &BlockId::Tag(Pending),
                ));
                Ok(CheatcodeHandlingResult::from_serializable(call_result))
            }
            "declare" => {
                let contract: String = input_reader.read::<ByteArray>()?.to_string();
                let fee_args: FeeArgs = input_reader.read::<ScriptFeeSettings>()?.into();
                let nonce = input_reader.read()?;

                let declare = Declare {
                    contract: contract.clone(),
                    fee_args,
                    nonce,
                    package: None,
                    rpc: RpcArgs::default(),
                };

                let declare_tx_id = generate_declare_tx_id(contract.as_str());

                if let Some(success_output) =
                    self.state.get_output_if_success(declare_tx_id.as_str())
                {
                    return Ok(CheatcodeHandlingResult::from_serializable(success_output));
                }

                let declare_result = self.tokio_runtime.block_on(declare::declare(
                    declare,
                    self.account()?,
                    self.artifacts,
                    WaitForTx {
                        wait: true,
                        wait_params: self.config.wait_params,
                    },
                    true,
                ));

                self.state.maybe_insert_tx_entry(
                    declare_tx_id.as_str(),
                    selector,
                    &declare_result,
                )?;
                Ok(CheatcodeHandlingResult::from_serializable(declare_result))
            }
            "deploy" => {
                let class_hash = input_reader.read()?;
                let constructor_calldata = input_reader.read::<Vec<Felt>>()?;
                let salt = input_reader.read()?;
                let unique = input_reader.read()?;
                let fee_args: FeeArgs = input_reader.read::<ScriptFeeSettings>()?.into();
                let nonce = input_reader.read()?;

                let deploy_tx_id =
                    generate_deploy_tx_id(class_hash, &constructor_calldata, salt, unique);

                if let Some(success_output) =
                    self.state.get_output_if_success(deploy_tx_id.as_str())
                {
                    return Ok(CheatcodeHandlingResult::from_serializable(success_output));
                }

                let deploy_result = self.tokio_runtime.block_on(deploy::deploy(
                    class_hash,
                    &constructor_calldata,
                    salt,
                    unique,
                    fee_args,
                    nonce,
                    self.account()?,
                    WaitForTx {
                        wait: true,
                        wait_params: self.config.wait_params,
                    },
                ));

                self.state.maybe_insert_tx_entry(
                    deploy_tx_id.as_str(),
                    selector,
                    &deploy_result,
                )?;

                Ok(CheatcodeHandlingResult::from_serializable(deploy_result))
            }
            "invoke" => {
                let contract_address = input_reader.read()?;
                let function_selector = input_reader.read()?;
                let calldata: Vec<_> = input_reader.read()?;
                let fee_args = input_reader.read::<ScriptFeeSettings>()?.into();
                let nonce = input_reader.read()?;

                let invoke_tx_id =
                    generate_invoke_tx_id(contract_address, function_selector, &calldata);

                if let Some(success_output) =
                    self.state.get_output_if_success(invoke_tx_id.as_str())
                {
                    return Ok(CheatcodeHandlingResult::from_serializable(success_output));
                }

                let invoke_result = self.tokio_runtime.block_on(invoke::invoke(
                    contract_address,
                    calldata,
                    nonce,
                    fee_args,
                    function_selector,
                    self.account()?,
                    WaitForTx {
                        wait: true,
                        wait_params: self.config.wait_params,
                    },
                ));

                self.state.maybe_insert_tx_entry(
                    invoke_tx_id.as_str(),
                    selector,
                    &invoke_result,
                )?;

                Ok(CheatcodeHandlingResult::from_serializable(invoke_result))
            }
            "get_nonce" => {
                let block_id = as_cairo_short_string(&input_reader.read()?)
                    .expect("Failed to convert entry point name to short string");

                let nonce = self.tokio_runtime.block_on(get_nonce(
                    self.provider,
                    &block_id,
                    self.account()?.address(),
                ))?;

                Ok(CheatcodeHandlingResult::from_serializable(nonce))
            }
            "tx_status" => {
                let transaction_hash = input_reader.read()?;

                let tx_status_result = self
                    .tokio_runtime
                    .block_on(tx_status::tx_status(self.provider, transaction_hash));

                Ok(CheatcodeHandlingResult::from_serializable(tx_status_result))
            }
            _ => Ok(CheatcodeHandlingResult::Forwarded),
        };

        res
    }

    fn override_system_call(
        &mut self,
        _selector: DeprecatedSyscallSelector,
        _vm: &mut VirtualMachine,
        _extended_runtime: &mut Self::Runtime,
    ) -> Result<SyscallHandlingResult, HintError> {
        Err(HintError::CustomHint(Box::from(
            "Starknet syscalls are not supported",
        )))
    }
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
pub fn run(
    module_name: &str,
    metadata: &Metadata,
    package_metadata: &PackageMetadata,
    artifacts: &mut HashMap<String, StarknetContractArtifacts>,
    provider: &JsonRpcClient<HttpTransport>,
    tokio_runtime: Runtime,
    config: &CastConfig,
    state_file_path: Option<Utf8PathBuf>,
) -> Result<ScriptRunResponse> {
    warn_if_sncast_std_not_compatible(metadata)?;
    let artifacts = inject_lib_artifact(metadata, package_metadata, artifacts)?;

    let artifact = artifacts
        .get(SCRIPT_LIB_ARTIFACT_NAME)
        .ok_or(anyhow!("Failed to find script artifact"))?;

    let sierra_program = serde_json::from_str::<VersionedProgram>(&artifact.sierra)
        .with_context(|| "Failed to deserialize Sierra program")?
        .into_v1()
        .with_context(|| "Failed to load Sierra program")?
        .program;

    let runner = SierraCasmRunner::new(
        sierra_program.clone(),
        Some(MetadataComputationConfig::default()),
        OrderedHashMap::default(),
        None,
    )
    .with_context(|| "Failed to set up runner")?;

    // `builder` field in `SierraCasmRunner` is private, hence the need to create a new `RunnableBuilder`
    // https://github.com/starkware-libs/cairo/blob/66f5c7223f7a6c27c5f800816dba05df9b60674e/crates/cairo-lang-runner/src/lib.rs#L184
    let builder = RunnableBuilder::new(sierra_program, Some(MetadataComputationConfig::default()))
        .with_context(|| "Failed to create builder")?;

    let name_suffix = module_name.to_string() + "::main";
    let func = runner.find_function(name_suffix.as_str())
        .context("Failed to find main function in script - please make sure `sierra-replace-ids` is not set to `false` for `dev` profile in script's Scarb.toml")?;

    let entry_code_config = EntryCodeConfig::testing();
    let (entry_code, builtins) = create_entry_code(&builder, func, entry_code_config)?;
    let footer = create_code_footer();

    // import from cairo-lang-runner
    let assembled_program = builder
        .casm_program()
        .clone()
        .assemble_ex(&entry_code, &footer);
    let (hints_dict, string_to_hint) = hints_to_params(assembled_program.hints);

    // hint processor
    let mut context = build_context(&SerializableBlockInfo::default().into(), None, &TrackedResource::CairoSteps);

    let mut blockifier_state = CachedState::new(DictStateReader::default());

    // TODO(#2954)
    let param_types = builder.generic_id_and_size_from_concrete(&func.signature.param_types);

    let segment_index = syscall_handler_offset(builtins.len(), has_segment_arena(&param_types));
    let syscall_handler = SyscallHintProcessor::new(
        &mut blockifier_state,
        &mut context,
        // This segment is created by SierraCasmRunner
        Relocatable {
            segment_index: segment_index
                .try_into()
                .expect("Failed to convert index to isize"),
            offset: 0,
        },
        CallEntryPoint::default(),
        &string_to_hint,
        ReadOnlySegments::default(),
    );

    let account = if config.account.is_empty() {
        None
    } else {
        Some(tokio_runtime.block_on(get_account(
            &config.account,
            &config.accounts_file,
            provider,
            config.keystore.clone(),
        ))?)
    };
    let state = StateManager::from(state_file_path)?;

    let cast_extension = CastScriptExtension {
        provider,
        tokio_runtime,
        config,
        artifacts: &artifacts,
        account: account.as_ref(),
        state,
    };

    let mut cast_runtime = ExtendedRuntime {
        extension: cast_extension,
        extended_runtime: StarknetRuntime {
            hint_handler: syscall_handler,
            // If for some reason we need to calculate `user_args`, here
            // is the function to do it: https://github.com/starkware-libs/cairo/blob/66f5c7223f7a6c27c5f800816dba05df9b60674e/crates/cairo-lang-runner/src/lib.rs#L464
            // See TODO(#2966)
            user_args: vec![vec![Arg::Value(Felt::from(i64::MAX))]],
        },
    };

    match runner.run_function(
        func,
        &mut cast_runtime,
        hints_dict,
        assembled_program.bytecode.iter(),
        builtins,
    ) {
        Ok(result) => match result.value {
            RunResultValue::Success(data) => Ok(ScriptRunResponse {
                status: "success".to_string(),
                message: build_readable_text(&data),
            }),
            RunResultValue::Panic(panic_data) => Ok(ScriptRunResponse {
                status: "script panicked".to_string(),
                message: build_readable_text(&panic_data),
            }),
        },
        Err(err) => Err(err.into()),
    }
}

fn sncast_std_version_requirement() -> VersionReq {
    let version = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
    let comparator = Comparator {
        op: Op::Exact,
        major: version.major,
        minor: Some(version.minor),
        patch: Some(version.patch),
        pre: version.pre,
    };
    VersionReq {
        comparators: vec![comparator],
    }
}

fn warn_if_sncast_std_not_compatible(scarb_metadata: &Metadata) -> Result<()> {
    let sncast_std_version_requirement = sncast_std_version_requirement();
    if !package_matches_version_requirement(
        scarb_metadata,
        "sncast_std",
        &sncast_std_version_requirement,
    )? {
        print_as_warning(&anyhow!("Package sncast_std version does not meet the recommended version requirement {sncast_std_version_requirement}, it might result in unexpected behaviour"));
    }
    Ok(())
}

fn inject_lib_artifact(
    metadata: &Metadata,
    package_metadata: &PackageMetadata,
    artifacts: &mut HashMap<String, StarknetContractArtifacts>,
) -> Result<HashMap<String, StarknetContractArtifacts>> {
    let sierra_filename = format!("{}.sierra.json", package_metadata.name);

    let target_dir = &metadata
        .target_dir
        .clone()
        .unwrap_or_else(|| metadata.workspace.root.join("target"));
    // TODO(#2042)
    let sierra_path = &target_dir.join("dev").join(sierra_filename);

    let lib_artifacts = ScriptStarknetContractArtifacts {
        sierra: fs::read_to_string(sierra_path)?,
        casm: String::new(),
    };

    artifacts.insert(SCRIPT_LIB_ARTIFACT_NAME.to_string(), lib_artifacts);
    Ok(artifacts.clone())
}

// Copied from https://github.com/starkware-libs/cairo/blob/66f5c7223f7a6c27c5f800816dba05df9b60674e/crates/cairo-lang-runnable-utils/src/builder.rs#L193
// TODO(#2953)
fn create_entry_code(
    builder: &RunnableBuilder,
    func: &Function,
    config: EntryCodeConfig,
) -> Result<(Vec<Instruction>, Vec<BuiltinName>), BuildError> {
    let param_types = builder.generic_id_and_size_from_concrete(&func.signature.param_types);
    let return_types = builder.generic_id_and_size_from_concrete(&func.signature.ret_types);

    let entry_point = func.entry_point.0;
    let code_offset =
        builder.casm_program().debug_info.sierra_statement_info[entry_point].start_offset;
    // Finalizing for proof only if all returned values are builtins or droppable.
    let droppable_return_value = func.signature.ret_types.iter().all(|ty| {
        let info = builder.registry().get_type(ty).unwrap().info();
        info.droppable || !builder.is_user_arg_type(&info.long_id.generic_id)
    });
    if !droppable_return_value {
        assert!(
            !config.finalize_segment_arena,
            "Cannot finalize the segment arena when returning non-droppable values."
        );
    }

    create_entry_code_from_params(&param_types, &return_types, code_offset, config)
}

fn hints_to_params(
    hints: Vec<(usize, Vec<Hint>)>,
) -> (HashMap<usize, Vec<HintParams>>, HashMap<String, Hint>) {
    let mut hints_dict: HashMap<usize, Vec<HintParams>> = HashMap::new();
    let mut string_to_hint: HashMap<String, Hint> = HashMap::new();

    for (offset, offset_hints) in hints {
        for hint in offset_hints.clone() {
            string_to_hint.insert(hint.representing_string(), hint.clone());
        }
        hints_dict.insert(
            offset,
            offset_hints
                .clone()
                .iter()
                .map(hint_to_hint_params)
                .collect(),
        );
    }

    (hints_dict, string_to_hint)
}
