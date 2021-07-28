use std::cell::{RefCell, RefMut};
use std::collections::HashMap;
use std::rc::Rc;
use colored::*;
use std::time::Instant;
use graph::blockchain::{Blockchain, HostFnCtx};
use graph::runtime::{HostExportError, FromAscObj};
use graph::semver::Version;
use wasmtime::{Trap};
use graph_runtime_wasm::error::DeterminismLevel;
pub use graph_runtime_wasm::host_exports;
use graph_runtime_wasm::mapping::MappingContext;
use anyhow::Error;
use graph::prelude::*;
use graph::runtime::{AscHeap, IndexForAscTypeId};
use graph::{components::subgraph::MappingError, runtime::AscPtr};
use graph::{
    data::subgraph::schema::SubgraphError,
    runtime::{asc_get, asc_new, try_asc_get, DeterministicHostError},
};
use graph_runtime_wasm::ExperimentalFeatures;
use graph_runtime_wasm::asc_abi::class::*;
use graph_runtime_wasm::mapping::ValidModule;
use graph_runtime_wasm::module::TimeoutStopwatch;
use graph_runtime_wasm::asc_abi::class::AscString;
use graph_runtime_wasm::module::IntoWasmRet;
use graph_runtime_wasm::module::WasmInstanceContext;
use ethabi::{Token, Address};
use indexmap::IndexMap;
use lazy_static::lazy_static;
use std::sync::Mutex;
use graph_chain_ethereum::runtime::abi::AscUnresolvedContractCall_0_0_4;

#[allow(unused)]
pub const TRAP_TIMEOUT: &str = "trap: interrupt";

pub trait IntoTrap {
    fn determinism_level(&self) -> DeterminismLevel;
    fn into_trap(self) -> Trap;
}

type Store = Mutex<IndexMap<String, IndexMap<String, HashMap<String, Value>>>>;

lazy_static! {
    static ref FUNCTIONS_MAP: Mutex<IndexMap<String, Token>> = Mutex::new(IndexMap::new());
    static ref STORE: Store = Mutex::from(IndexMap::new());
    pub static ref LOGS: Mutex<IndexMap<String, Level>> = Mutex::new(IndexMap::new());
    pub static ref TEST_RESULTS: Mutex<IndexMap<String, bool>> = Mutex::new(IndexMap::new());
}

pub enum Level {
    ERROR,
    WARNING,
    INFO,
    DEBUG,
    SUCCESS,
    UNKNOWN,
}

fn level_from_u32(n: u32) -> Level {
    match n {
        1 => Level::ERROR,
        2 => Level::WARNING,
        3 => Level::INFO,
        4 => Level::DEBUG,
        5 => Level::SUCCESS,
        _ => Level::UNKNOWN,
    }
}

pub fn get_successful_tests() -> usize {
    let map = TEST_RESULTS.lock().expect("Cannot access TEST_RESULTS.");
    map.iter().filter(|(_, &v)| v).count()
}

pub fn get_failed_tests() -> usize {
    let map = TEST_RESULTS.lock().expect("Cannot access TEST_RESULTS.");
    map.iter().filter(|(_, &v)| !v).count()
}

fn styled(s: &str, n: &Level) -> ColoredString {
    match n {
        Level::ERROR => format!("ERROR {}", s).red(),
        Level::WARNING => format!("WARNING {}", s).yellow(),
        Level::INFO => format!("INFO {}", s).normal(),
        Level::DEBUG => format!("DEBUG {}", s).cyan(),
        Level::SUCCESS => format!("SUCCESS {}", s).green(),
        _ => s.normal(),
    }
}

pub fn fail_test(msg: String) {
    let test_name = TEST_RESULTS
        .lock()
        .expect("Cannot access TEST_RESULTS.")
        .keys()
        .last()
        .unwrap()
        .clone();
    TEST_RESULTS
        .lock()
        .expect("Cannot access TEST_RESULTS.")
        .insert(test_name, false);
    LOGS.lock()
        .expect("Cannot access LOGS.")
        .insert(msg, Level::ERROR);
}

struct UnresolvedContractCall {
    pub contract_name: String,
    pub contract_address: Address,
    pub function_name: String,
    pub function_signature: Option<String>,
    pub function_args: Vec<Token>,
}

pub fn flush_logs() {
    let test_results = TEST_RESULTS.lock().expect("Cannot access TEST_RESULTS.");
    let logs = LOGS.lock().expect("Cannot access LOGS.");

    for (k, v) in logs.iter() {
        // Test name
        if test_results.contains_key(k) {
            let passed = *test_results.get(k).unwrap();

            if passed {
                println!("✅ {}", k.green());
            } else {
                println!("❌ {}", k.red());
            }
        }
        // Normal log
        else {
            println!("{}", styled(k, v));
        }
    }
}

trait WICExtension {
    fn log(&mut self, level: u32, msg: AscPtr<AscString>) -> Result<(), HostExportError>;
    fn clear_store(&mut self) -> Result<(), HostExportError>;
    fn register_test(&mut self, name: AscPtr<AscString>) -> Result<(), HostExportError>;
    fn assert_field_equals(
        &mut self,
        entity_type_ptr: AscPtr<AscString>,
        id_ptr: AscPtr<AscString>,
        field_name_ptr: AscPtr<AscString>,
        expected_val_ptr: AscPtr<AscString>,
    ) -> Result<(), HostExportError>;
    fn mock_store_get(
        &mut self,
        entity_type_ptr: AscPtr<AscString>,
        id_ptr: AscPtr<AscString>,
    ) -> Result<AscPtr<AscEntity>, HostExportError>;
    fn mock_store_set(
        &mut self,
        entity_type_ptr: AscPtr<AscString>,
        id_ptr: AscPtr<AscString>,
        data_ptr: AscPtr<AscEntity>,
    ) -> Result<(), HostExportError>;
    fn mock_store_remove(
        &mut self,
        entity_type_ptr: AscPtr<AscString>,
        id_ptr: AscPtr<AscString>,
    ) -> Result<(), HostExportError>;
    fn ethereum_call(
        &mut self,
        contract_call_ptr: u32,
    ) -> Result<AscEnumArray<EthereumValueKind>, HostExportError>;
    fn mock_function(
        &mut self,
        contract_address_ptr: u32,
        fn_name_ptr: AscPtr<AscString>,
        fn_args_ptr: u32,
        return_value_ptr: u32,
    ) -> Result<(), HostExportError>;
}

impl FromAscObj<AscUnresolvedContractCall_0_0_4> for UnresolvedContractCall {
    fn from_asc_obj<H: AscHeap + ?Sized>(
        asc_call: AscUnresolvedContractCall_0_0_4,
        heap: &H,
    ) -> Result<Self, DeterministicHostError> {
        Ok(UnresolvedContractCall {
            contract_name: asc_get(heap, asc_call.contract_name)?,
            contract_address: asc_get(heap, asc_call.contract_address)?,
            function_name: asc_get(heap, asc_call.function_name)?,
            function_signature: Some(asc_get(heap, asc_call.function_signature)?),
            function_args: asc_get(heap, asc_call.function_args)?,
        })
    }
}

impl<C: Blockchain> WICExtension for WasmInstanceContext<C> {
    fn log(&mut self, level: u32, msg: AscPtr<AscString>) -> Result<(), HostExportError> {
        let msg: String = asc_get(self, msg)?;

        match level {
            // CRITICAL (for expected logic errors)
            0 => {
                panic!("❌ ❌ ❌ {}", msg.red());
            }
            1 => {
                fail_test(msg);
            }
            _ => {
                LOGS.lock()
                    .expect("Cannot access LOGS.")
                    .insert(msg, level_from_u32(level));
            }
        }

        Ok(())
    }

    fn clear_store(&mut self) -> Result<(), HostExportError> {
        STORE.lock().expect("Cannot access STORE.").clear();
        Ok(())
    }

    fn register_test(&mut self, name_ptr: AscPtr<AscString>) -> Result<(), HostExportError> {
        let name: String = asc_get(self, name_ptr)?;

        if TEST_RESULTS
            .lock()
            .expect("Cannot access TEST_RESULTS.")
            .contains_key(&name)
        {
            let msg = format!("❌ ❌ ❌  Test with name '{}' already exists.", name).red();
            panic!("{}", msg);
        }

        TEST_RESULTS
            .lock()
            .expect("Cannot access TEST_RESULTS.")
            .insert(name.clone(), true);
        LOGS.lock()
            .expect("Cannot access LOGS.")
            .insert(name, Level::INFO);

        Ok(())
    }

    fn assert_field_equals(
        &mut self,
        entity_type_ptr: AscPtr<AscString>,
        id_ptr: AscPtr<AscString>,
        field_name_ptr: AscPtr<AscString>,
        expected_val_ptr: AscPtr<AscString>,
    ) -> Result<(), HostExportError> {
        let entity_type: String = asc_get(self, entity_type_ptr)?;
        let id: String = asc_get(self, id_ptr)?;
        let field_name: String = asc_get(self, field_name_ptr)?;
        let expected_val: String = asc_get(self, expected_val_ptr)?;

        let map = STORE.lock().expect("Cannot access STORE.");
        if !map.contains_key(&entity_type) {
            let msg = format!(
                "(assert.fieldEquals) No entities with type '{}' found.",
                &entity_type
            );
            fail_test(msg);
            return Ok(());
        }

        let entities = map.get(&entity_type).unwrap();
        if !entities.contains_key(&id) {
            let msg = format!(
                "(assert.fieldEquals) No entity with type '{}' and id '{}' found.",
                &entity_type, &id
            );
            fail_test(msg);
            return Ok(());
        }

        let entity = entities.get(&id).unwrap();
        if !entity.contains_key(&field_name) {
            let msg = format!(
                "(assert.fieldEquals) No field named '{}' on entity with type '{}' and id '{}' found.",
                &field_name, &entity_type, &id
            );
            fail_test(msg);
            return Ok(());
        }

        let val = entity.get(&field_name).unwrap();
        if val.to_string() != expected_val {
            let msg = format!(
                "(assert.fieldEquals) Expected field '{}' to equal '{}', but was '{}' instead.",
                &field_name, &expected_val, val
            );
            fail_test(msg);
            return Ok(());
        };

        Ok(())
    }

    fn mock_store_get(
        &mut self,
        entity_type_ptr: AscPtr<AscString>,
        id_ptr: AscPtr<AscString>,
    ) -> Result<AscPtr<AscEntity>, HostExportError> {
        let entity_type: String = asc_get(self, entity_type_ptr)?;
        let id: String = asc_get(self, id_ptr)?;

        let map = STORE.lock().expect("Cannot access STORE.");

        if map.contains_key(&entity_type) && map.get(&entity_type).unwrap().contains_key(&id) {
            let entities = map.get(&entity_type).unwrap();
            let entity = entities.get(&id).unwrap().clone();
            let entity = Entity::from(entity);

            let res = asc_new(self, &entity.sorted())?;
            return Ok(res);
        }

        Ok(AscPtr::null())
    }

    fn mock_store_set(
        &mut self,
        entity_type_ptr: AscPtr<AscString>,
        id_ptr: AscPtr<AscString>,
        data_ptr: AscPtr<AscEntity>,
    ) -> Result<(), HostExportError> {
        let entity_type: String = asc_get(self, entity_type_ptr)?;
        let id: String = asc_get(self, id_ptr)?;
        let data: HashMap<String, Value> = try_asc_get(self, data_ptr)?;

        let mut map = STORE.lock().expect("Cannot get STORE.");
        let mut inner_map = if map.contains_key(&entity_type) {
            map.get(&entity_type).unwrap().clone()
        } else {
            IndexMap::new()
        };

        inner_map.insert(id, data);
        map.insert(entity_type, inner_map);
        Ok(())
    }

    fn mock_store_remove(
        &mut self,
        entity_type_ptr: AscPtr<AscString>,
        id_ptr: AscPtr<AscString>,
    ) -> Result<(), HostExportError> {
        let entity_type: String = asc_get(self, entity_type_ptr)?;
        let id: String = asc_get(self, id_ptr)?;

        let mut map = STORE.lock().unwrap();
        if map.contains_key(&entity_type) && map.get(&entity_type).unwrap().contains_key(&id) {
            let mut inner_map = map.get(&entity_type).unwrap().clone();
            inner_map.remove(&id);

            map.insert(entity_type, inner_map);
        } else {
            let msg = format!(
                "(store.remove) Entity with type '{}' and id '{}' does not exist. Problem originated from store.remove()",
                &entity_type, &id
            );
            fail_test(msg);
            return Ok(());
        }
        Ok(())
    }

    fn ethereum_call(
        &mut self,
        contract_call_ptr: u32,
    ) -> Result<AscEnumArray<EthereumValueKind>, HostExportError> {
        let call: UnresolvedContractCall =
            asc_get::<_, AscUnresolvedContractCall_0_0_4, _>(self, contract_call_ptr.into())?;

        let unique_fn_string = create_unique_fn_string(
            &call.contract_address.to_string(),
            &call.function_name,
            call.function_args,
        );
        let map = FUNCTIONS_MAP.lock().expect("Couldn't get map");
        let return_val;
        if map.contains_key(&unique_fn_string) {
            return_val = asc_new(
                self,
                vec![map
                    .get(&unique_fn_string)
                    .expect("Couldn't get value from map.")]
                    .as_slice(),
            )?;
        } else {
            panic!("key: '{}' not found in map.", &unique_fn_string);
        }
        Ok(return_val)
    }

    fn mock_function(
        &mut self,
        contract_address_ptr: u32,
        fn_name_ptr: AscPtr<AscString>,
        fn_args_ptr: u32,
        return_value_ptr: u32,
    ) -> Result<(), HostExportError> {
        let contract_address: Address = asc_get(self, contract_address_ptr.into())?;
        let fn_name: String = asc_get(self, fn_name_ptr)?;
        let fn_args: Vec<Token> =
            asc_get::<_, Array<AscPtr<AscEnum<EthereumValueKind>>>, _>(self, fn_args_ptr.into())?;
        let return_value: Token =
            asc_get::<_, AscEnum<EthereumValueKind>, _>(self, return_value_ptr.into())?;

        let unique_fn_string =
            create_unique_fn_string(&contract_address.to_string(), &fn_name, fn_args);
        let mut map = FUNCTIONS_MAP.lock().expect("Couldn't get map");
        map.insert(unique_fn_string, return_value);
        Ok(())
    }
}

fn create_unique_fn_string(contract_address: &str, fn_name: &str, fn_args: Vec<Token>) -> String {
    let mut unique_fn_string = String::from(contract_address) + fn_name;
    for element in fn_args.iter() {
        unique_fn_string += &element.to_string();
    }
    return unique_fn_string;
}

pub struct WasmInstance<C: Blockchain> {
    pub instance: wasmtime::Instance,
    instance_ctx: Rc<RefCell<Option<WasmInstanceContext<C>>>>,
}

impl<C: Blockchain> Drop for WasmInstance<C> {
    fn drop(&mut self) {
        assert_eq!(Rc::strong_count(&self.instance_ctx), 1);
    }
}

impl<C: Blockchain> AscHeap for WasmInstance<C> {
    fn raw_new(&mut self, bytes: &[u8]) -> Result<u32, DeterministicHostError> {
        let mut ctx = RefMut::map(self.instance_ctx.borrow_mut(), |i| i.as_mut().unwrap());
        ctx.raw_new(bytes)
    }

    fn get(&self, offset: u32, size: u32) -> Result<Vec<u8>, DeterministicHostError> {
        self.instance_ctx().get(offset, size)
    }

    fn api_version(&self) -> Version {
        self.instance_ctx().api_version()
    }

    fn asc_type_id(
        &mut self,
        type_id_index: IndexForAscTypeId,
    ) -> Result<u32, DeterministicHostError> {
        self.instance_ctx_mut().asc_type_id(type_id_index)
    }
}

impl<C: Blockchain> WasmInstance<C> {
    #[allow(unused)]
    pub fn take_ctx(&mut self) -> WasmInstanceContext<C> {
        self.instance_ctx.borrow_mut().take().unwrap()
    }

    pub(crate) fn instance_ctx(&self) -> std::cell::Ref<'_, WasmInstanceContext<C>> {
        std::cell::Ref::map(self.instance_ctx.borrow(), |i| i.as_ref().unwrap())
    }

    pub fn instance_ctx_mut(&self) -> std::cell::RefMut<'_, WasmInstanceContext<C>> {
        std::cell::RefMut::map(self.instance_ctx.borrow_mut(), |i| i.as_mut().unwrap())
    }

    #[cfg(debug_assertions)]
    // pub fn get_func(&self, func_name: &str) -> wasmtime::Func {
    //     self.instance.get_func(func_name).unwrap()
    // }

    fn invoke_handler<T>(
        &mut self,
        handler: &str,
        arg: AscPtr<T>,
    ) -> Result<BlockState<C>, MappingError> {
        let func = self
            .instance
            .get_func(handler)
            .with_context(|| format!("function {} not found", handler))?;

        // Caution: Make sure all exit paths from this function call `exit_handler`.
        self.instance_ctx_mut().ctx.state.enter_handler();

        // This `match` will return early if there was a non-deterministic trap.
        let deterministic_error: Option<Error> = match func.typed()?.call(arg.wasm_ptr()) {
            Ok(()) => None,
            Err(trap) if self.instance_ctx().possible_reorg => {
                self.instance_ctx_mut().ctx.state.exit_handler();
                return Err(MappingError::PossibleReorg(trap.into()));
            }
            Err(trap) if trap.to_string().contains(TRAP_TIMEOUT) => {
                self.instance_ctx_mut().ctx.state.exit_handler();
                return Err(MappingError::Unknown(Error::from(trap).context(format!(
                    "Handler '{}' hit the timeout of '{}' seconds",
                    handler,
                    self.instance_ctx().timeout.unwrap().as_secs()
                ))));
            }
            Err(trap) => {
                use wasmtime::TrapCode::*;
                let trap_code = trap.trap_code();
                let e = Error::from(trap);
                match trap_code {
                    Some(MemoryOutOfBounds)
                    | Some(HeapMisaligned)
                    | Some(TableOutOfBounds)
                    | Some(IndirectCallToNull)
                    | Some(BadSignature)
                    | Some(IntegerOverflow)
                    | Some(IntegerDivisionByZero)
                    | Some(BadConversionToInteger)
                    | Some(UnreachableCodeReached) => Some(e),
                    _ if self.instance_ctx().deterministic_host_trap => Some(e),
                    _ => {
                        self.instance_ctx_mut().ctx.state.exit_handler();
                        return Err(MappingError::Unknown(e));
                    }
                }
            }
        };

        if let Some(deterministic_error) = deterministic_error {
            let message = format!("{:#}", deterministic_error).replace("\n", "\t");

            // Log the error and restore the updates snapshot, effectively reverting the handler.
            error!(&self.instance_ctx().ctx.logger,
                "Handler skipped due to execution failure";
                "handler" => handler,
                "error" => &message,
            );
            let subgraph_error = SubgraphError {
                subgraph_id: self.instance_ctx().ctx.host_exports.subgraph_id.clone(),
                message,
                block_ptr: Some(self.instance_ctx().ctx.block_ptr.cheap_clone()),
                handler: Some(handler.to_string()),
                deterministic: true,
            };
            self.instance_ctx_mut()
                .ctx
                .state
                .exit_handler_and_discard_changes_due_to_error(subgraph_error);
        } else {
            self.instance_ctx_mut().ctx.state.exit_handler();
        }

        Ok(self.take_ctx().ctx.state)
    }
}

impl IntoTrap for DeterministicHostError {
    fn determinism_level(&self) -> DeterminismLevel {
        unreachable!();
    }

    fn into_trap(self) -> Trap {
        unreachable!();
    }
}

impl IntoTrap for HostExportError {
    fn determinism_level(&self) -> DeterminismLevel {
        unreachable!();
    }

    fn into_trap(self) -> Trap {
        unreachable!();
    }
}

impl<C: Blockchain> WasmInstance<C> {
    /// Instantiates the module and sets it to be interrupted after `timeout`.
    pub fn from_valid_module_with_ctx(
        valid_module: Arc<ValidModule>,
        ctx: MappingContext<C>,
        host_metrics: Arc<HostMetrics>,
        timeout: Option<Duration>,
        experimental_features: ExperimentalFeatures,
    ) -> Result<WasmInstance<C>, anyhow::Error> {
        let mut linker = wasmtime::Linker::new(&wasmtime::Store::new(valid_module.module.engine()));
        let host_fns = ctx.host_fns.cheap_clone();
        let api_version = ctx.host_exports.api_version.clone();

        // Used by exports to access the instance context. There are two ways this can be set:
        // - After instantiation, if no host export is called in the start function.
        // - During the start function, if it calls a host export.
        // Either way, after instantiation this will have been set.
        let shared_ctx: Rc<RefCell<Option<WasmInstanceContext<C>>>> = Rc::new(RefCell::new(None));

        // We will move the ctx only once, to init `shared_ctx`. But we don't statically know where
        // it will be moved so we need this ugly thing.
        let ctx: Rc<RefCell<Option<MappingContext<C>>>> = Rc::new(RefCell::new(Some(ctx)));

        // Start the timeout watchdog task.
        let timeout_stopwatch = Arc::new(std::sync::Mutex::new(TimeoutStopwatch::start_new()));
        if let Some(timeout) = timeout {
            // This task is likely to outlive the instance, which is fine.
            let interrupt_handle = linker.store().interrupt_handle().unwrap();
            let timeout_stopwatch = timeout_stopwatch.clone();
            graph::spawn_allow_panic(async move {
                let minimum_wait = Duration::from_secs(1);
                loop {
                    let time_left =
                        timeout.checked_sub(timeout_stopwatch.lock().unwrap().elapsed());
                    match time_left {
                        None => break interrupt_handle.interrupt(), // Timed out.

                        Some(time) if time < minimum_wait => break interrupt_handle.interrupt(),
                        Some(time) => tokio::time::delay_for(time).await,
                    }
                }
            });
        }

        macro_rules! link {
            ($wasm_name:expr, $rust_name:ident, $($param:ident),*) => {
                link!($wasm_name, $rust_name, "host_export_other", $($param),*)
            };

            ($wasm_name:expr, $rust_name:ident, $section:expr, $($param:ident),*) => {
                let modules = valid_module
                    .import_name_to_modules
                    .get($wasm_name)
                    .into_iter()
                    .flatten();

                // link an import with all the modules that require it.
                for module in modules {
                    let func_shared_ctx = Rc::downgrade(&shared_ctx);
                    let valid_module = valid_module.cheap_clone();
                    let host_metrics = host_metrics.cheap_clone();
                    let timeout_stopwatch = timeout_stopwatch.cheap_clone();
                    let ctx = ctx.cheap_clone();
                    linker.func(
                        module,
                        $wasm_name,
                        move |caller: wasmtime::Caller, $($param: u32),*| {
                            let instance = func_shared_ctx.upgrade().unwrap();
                            let mut instance = instance.borrow_mut();

                            // Happens when calling a host fn in Wasm start.
                            if instance.is_none() {
                                *instance = Some(WasmInstanceContext::from_caller(
                                    caller,
                                    ctx.borrow_mut().take().unwrap(),
                                    valid_module.cheap_clone(),
                                    host_metrics.cheap_clone(),
                                    timeout,
                                    timeout_stopwatch.cheap_clone(),
                                    experimental_features.clone()
                                ).unwrap())
                            }

                            let instance = instance.as_mut().unwrap();
                            let _section = instance.host_metrics.stopwatch.start_section($section);

                            let result = instance.$rust_name(
                                $($param.into()),*
                            );
                            match result {
                                Ok(result) => Ok(result.into_wasm_ret()),
                                Err(e) => {
                                    match IntoTrap::determinism_level(&e) {
                                        DeterminismLevel::Deterministic => {
                                            instance.deterministic_host_trap = true;
                                        },
                                        DeterminismLevel::PossibleReorg => {
                                            instance.possible_reorg = true;
                                        },
                                        DeterminismLevel::Unimplemented | DeterminismLevel::NonDeterministic => {},
                                    }

                                    Err(IntoTrap::into_trap(e))
                                }
                            }
                        }
                    )?;
                }
            };
        }

        // Link chain-specifc host fns.
        for host_fn in host_fns.iter() {
            let modules = valid_module
                .import_name_to_modules
                .get(host_fn.name)
                .into_iter()
                .flatten();

            for module in modules {
                let func_shared_ctx = Rc::downgrade(&shared_ctx);
                let host_fn = host_fn.cheap_clone();
                linker.func(module, host_fn.name, move |call_ptr: u32| {
                    let start = Instant::now();
                    let instance = func_shared_ctx.upgrade().unwrap();
                    let mut instance = instance.borrow_mut();

                    let instance = match &mut *instance {
                        Some(instance) => instance,

                        // Happens when calling a host fn in Wasm start.
                        None => {
                            return Err(anyhow!(
                                "{} is not allowed in global variables",
                                host_fn.name
                            )
                                .into())
                        }
                    };

                    let name_for_metrics = host_fn.name.replace('.', "_");
                    let stopwatch = &instance.host_metrics.stopwatch;
                    let _section =
                        stopwatch.start_section(&format!("host_export_{}", name_for_metrics));

                    let ctx = HostFnCtx {
                        logger: instance.ctx.logger.cheap_clone(),
                        block_ptr: instance.ctx.block_ptr.cheap_clone(),
                        heap: instance,
                    };
                    let ret = (host_fn.func)(ctx, call_ptr).map_err(|e| match e {
                        HostExportError::Deterministic(e) => {
                            instance.deterministic_host_trap = true;
                            e
                        }
                        HostExportError::PossibleReorg(e) => {
                            instance.possible_reorg = true;
                            e
                        }
                        HostExportError::Unknown(e) => e,
                    })?;
                    instance.host_metrics.observe_host_fn_execution_time(
                        start.elapsed().as_secs_f64(),
                        &name_for_metrics,
                    );
                    Ok(ret)
                })?;
            }
        }

        link!("ethereum.call", ethereum_call, contract_call_ptr);
        link!("ethereum.encode", ethereum_encode, params_ptr);
        link!("ethereum.decode", ethereum_decode, params_ptr, data_ptr);

        link!("abort", abort, message_ptr, file_name_ptr, line, column);

        link!(
            "mockFunction",
            mock_function,
            contract_address_ptr,
            fn_name_ptr,
            fn_args_ptr,
            return_value_ptr
        );

        link!("clearStore", clear_store,);
        link!("store.get", mock_store_get, "host_export_store_get", entity, id);
        link!(
            "store.set",
            mock_store_set,
            "host_export_store_set",
            entity,
            id,
            data
        );

        link!("ipfs.cat", ipfs_cat, "host_export_ipfs_cat", hash_ptr);
        link!(
            "ipfs.map",
            ipfs_map,
            "host_export_ipfs_map",
            link_ptr,
            callback,
            user_data,
            flags
        );

        link!("store.remove", mock_store_remove, entity_ptr, id_ptr);

        link!("typeConversion.bytesToString", bytes_to_string, ptr);
        link!("typeConversion.bytesToHex", bytes_to_hex, ptr);
        link!("typeConversion.bigIntToString", big_int_to_string, ptr);
        link!("typeConversion.bigIntToHex", big_int_to_hex, ptr);
        link!("typeConversion.stringToH160", string_to_h160, ptr);
        link!("typeConversion.bytesToBase58", bytes_to_base58, ptr);

        link!("json.fromBytes", json_from_bytes, ptr);
        link!("json.try_fromBytes", json_try_from_bytes, ptr);
        link!("json.toI64", json_to_i64, ptr);
        link!("json.toU64", json_to_u64, ptr);
        link!("json.toF64", json_to_f64, ptr);
        link!("json.toBigInt", json_to_big_int, ptr);

        link!("crypto.keccak256", crypto_keccak_256, ptr);

        link!("bigInt.plus", big_int_plus, x_ptr, y_ptr);
        link!("bigInt.minus", big_int_minus, x_ptr, y_ptr);
        link!("bigInt.times", big_int_times, x_ptr, y_ptr);
        link!("bigInt.dividedBy", big_int_divided_by, x_ptr, y_ptr);
        link!("bigInt.dividedByDecimal", big_int_divided_by_decimal, x, y);
        link!("bigInt.mod", big_int_mod, x_ptr, y_ptr);
        link!("bigInt.pow", big_int_pow, x_ptr, exp);
        link!("bigInt.fromString", big_int_from_string, ptr);
        link!("bigInt.bitOr", big_int_bit_or, x_ptr, y_ptr);
        link!("bigInt.bitAnd", big_int_bit_and, x_ptr, y_ptr);
        link!("bigInt.leftShift", big_int_left_shift, x_ptr, bits);
        link!("bigInt.rightShift", big_int_right_shift, x_ptr, bits);

        link!("bigDecimal.toString", big_decimal_to_string, ptr);
        link!("bigDecimal.fromString", big_decimal_from_string, ptr);
        link!("bigDecimal.plus", big_decimal_plus, x_ptr, y_ptr);
        link!("bigDecimal.minus", big_decimal_minus, x_ptr, y_ptr);
        link!("bigDecimal.times", big_decimal_times, x_ptr, y_ptr);
        link!("bigDecimal.dividedBy", big_decimal_divided_by, x, y);
        link!("bigDecimal.equals", big_decimal_equals, x_ptr, y_ptr);

        link!("dataSource.create", data_source_create, name, params);
        link!(
            "dataSource.createWithContext",
            data_source_create_with_context,
            name,
            params,
            context
        );
        link!("dataSource.address", data_source_address,);
        link!("dataSource.network", data_source_network,);
        link!("dataSource.context", data_source_context,);

        link!("ens.nameByHash", ens_name_by_hash, ptr);

        link!("log.log", log_log, level, msg_ptr);

        link!("registerTest", register_test, name_ptr);

        link!(
            "assert.fieldEquals",
            assert_field_equals,
            entity_type_ptr,
            id_ptr,
            field_name_ptr,
            expected_val_ptr
        );

        // `arweave and `box` functionality was removed, but apiVersion <= 0.0.4 must link it.
        if api_version <= Version::new(0, 0, 4) {
            link!("arweave.transactionData", arweave_transaction_data, ptr);
            link!("box.profile", box_profile, ptr);
        }

        let instance = linker.instantiate(&valid_module.module)?;

        // Usually `shared_ctx` is still `None` because no host fns were called during start.
        if shared_ctx.borrow().is_none() {
            *shared_ctx.borrow_mut() = Some(WasmInstanceContext::from_instance(
                &instance,
                ctx.borrow_mut().take().unwrap(),
                valid_module,
                host_metrics,
                timeout,
                timeout_stopwatch,
                experimental_features,
            )?);
        }

        match api_version {
            version if version <= Version::new(0, 0, 4) => {}
            _ => {
                instance
                    .get_func("_start")
                    .context("`_start` function not found")?
                    .typed::<(), ()>()?
                    .call(())
                    .unwrap();
            }
        }

        Ok(WasmInstance {
            instance,
            instance_ctx: shared_ctx,
        })
    }
}
