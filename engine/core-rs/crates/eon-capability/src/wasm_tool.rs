//! WebAssembly tool execution with fuel metering and no host imports (docs/implementation/01, section 5, sandbox layer).
//!
//! A tool module runs under a fixed fuel budget. Running out of fuel is reported as `ResourceExhausted`. The linker is
//! empty, so a module that declares any import fails to instantiate, which means it gets no WASI, no filesystem, no
//! network, and no clock.

use std::sync::OnceLock;

use eon_primitives::{EonError, EonResult};
use wasmtime::{
    Config, Engine, ExternType, Instance, Linker, Module, Store, StoreLimits, StoreLimitsBuilder,
    Trap, ValType,
};

mod worker;
#[doc(hidden)]
pub use worker::worker_main;
pub use worker::{MAX_WORKER_WALL_TIME, WasmWorker};

const MAX_LINEAR_MEMORY: usize = 64 * 1024 * 1024;
pub const MAX_INPUT_BYTES: usize = 16 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024;
pub const MAX_MODULE_BYTES: usize = 256 * 1024;
pub const DISPATCH_FUEL: u64 = 100_000;

pub(crate) fn check_size(resource: &str, size: usize, limit: usize) -> EonResult<()> {
    if size > limit {
        return Err(EonError::ResourceExhausted {
            resource: resource.into(),
            requested: size as u64,
            available: limit as u64,
        });
    }
    Ok(())
}

fn operation_failed(operation: &str, reason: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: operation.to_owned(),
        reason: reason.to_string(),
    }
}

fn config() -> Config {
    let mut config = Config::new();
    // The limit is per linear memory; shared memories do not use the store's growth limiter.
    config
        .consume_fuel(true)
        .wasm_multi_memory(false)
        .wasm_threads(false)
        .wasm_gc(false)
        .wasm_exceptions(false);
    config
}

fn engine() -> EonResult<&'static Engine> {
    static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();
    ENGINE
        .get_or_init(|| Engine::new(&config()).map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| operation_failed("wasm.engine", e))
}

/// Compiles `module` (binary or WAT text), instantiates it with no imports, and calls its exported `(i32) -> i32`
/// function `export` with `arg` under a budget of `fuel` instructions.
/// This low-level in-process helper has no host compilation or wall deadline. Not for registry dispatch.
pub fn call_i32_export(module: &[u8], export: &str, arg: i32, fuel: u64) -> EonResult<i32> {
    let module = Module::new(engine()?, module).map_err(|e| operation_failed("wasm.compile", e))?;
    call_module(&module, export, arg, fuel)
}

fn execution_error(operation: &str, e: wasmtime::Error, fuel: u64) -> EonError {
    if matches!(e.downcast_ref::<Trap>(), Some(Trap::OutOfFuel)) {
        EonError::ResourceExhausted {
            resource: "wasm_fuel".into(),
            requested: fuel,
            available: 0,
        }
    } else if let Some(trap) = e.downcast_ref::<Trap>() {
        // A guest backtrace can contain arbitrary module/function names. Report
        // the trap kind, not guest-controlled text or a potentially large trace.
        operation_failed(operation, trap)
    } else {
        operation_failed(operation, format!("{e:#}"))
    }
}

fn instantiate(module: &Module, fuel: u64) -> EonResult<(Store<StoreLimits>, Instance)> {
    let engine = module.engine();
    let limits = StoreLimitsBuilder::new()
        .memory_size(MAX_LINEAR_MEMORY)
        .memories(1)
        .instances(1)
        .tables(1)
        .table_elements(1024)
        .build();
    let mut store = Store::new(engine, limits);
    store.limiter(|limits| limits);
    store
        .set_fuel(fuel)
        .map_err(|e| operation_failed("wasm.fuel", e))?;
    let instance = Linker::new(engine)
        .instantiate(&mut store, module)
        .map_err(|e| execution_error("wasm.instantiate", e, fuel))?;
    Ok((store, instance))
}

fn call_module(module: &Module, export: &str, arg: i32, fuel: u64) -> EonResult<i32> {
    let (mut store, instance) = instantiate(module, fuel)?;
    let func = instance
        .get_typed_func::<i32, i32>(&mut store, export)
        .map_err(|e| operation_failed("wasm.export", e))?;
    func.call(&mut store, arg)
        .map_err(|e| execution_error("wasm.call", e, fuel))
}

/// Registry ABI: canonical JSON at memory offset zero; `run(input_len)` returns the
/// unsigned byte length of UTF-8 output at the same offset. Neither buffer is a host pointer.
struct PureModule(Module);

impl PureModule {
    fn compile(bytes: &[u8]) -> EonResult<Self> {
        let engine = Engine::new(
            config()
                .parallel_compilation(false)
                .memory_reservation(MAX_LINEAR_MEMORY as u64),
        )
        .map_err(|e| operation_failed("wasm.engine", e))?;
        let module =
            Module::new(&engine, bytes).map_err(|e| operation_failed("wasm.compile", e))?;
        if module.imports().next().is_some() {
            return Err(EonError::SecurityViolation(
                "pure tools cannot import host capabilities".into(),
            ));
        }
        let signature_ok = matches!(module.get_export("run"), Some(ExternType::Func(f))
            if f.params().len() == 1 && matches!(f.param(0), Some(ValType::I32))
            && f.results().len() == 1 && matches!(f.result(0), Some(ValType::I32)));
        if !signature_ok
            || !matches!(module.get_export("memory"), Some(ExternType::Memory(m)) if !m.is_64())
        {
            return Err(EonError::ValidationError(
                "expected exported memory32 and run(i32) -> i32".into(),
            ));
        }
        Ok(Self(module))
    }

    fn run(&self, input: &str) -> EonResult<String> {
        check_size("tool_input_bytes", input.len(), MAX_INPUT_BYTES)?;
        let (mut store, instance) = instantiate(&self.0, DISPATCH_FUEL)?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| operation_failed("wasm.export", "missing memory"))?;
        memory
            .write(&mut store, 0, input.as_bytes())
            .map_err(|e| operation_failed("wasm.input", e))?;
        let func = instance
            .get_typed_func::<i32, i32>(&mut store, "run")
            .map_err(|e| operation_failed("wasm.export", e))?;
        let len = func
            .call(&mut store, input.len() as i32)
            .map_err(|e| execution_error("wasm.call", e, DISPATCH_FUEL))? as u32
            as usize;
        // Check the budget and guest bounds *before* allocating or copying any host output.
        check_size("tool_output_bytes", len, MAX_OUTPUT_BYTES)?;
        let bytes = memory
            .data(&store)
            .get(..len)
            .ok_or_else(|| operation_failed("wasm.output", "output exceeds guest memory"))?;
        let output = std::str::from_utf8(bytes)
            .map_err(|_| operation_failed("wasm.output", "output is not UTF-8"))?;
        Ok(output.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADD_TWO: &str = r#"(module (func (export "run") (param i32) (result i32) local.get 0 i32.const 2 i32.add))"#;

    #[test]
    fn a_pure_tool_runs_and_returns_its_result() {
        assert_eq!(
            call_i32_export(ADD_TWO.as_bytes(), "run", 40, 1_000_000).unwrap(),
            42
        );
    }

    #[test]
    fn fuel_stop_releases_the_instance_and_memory_for_a_clean_call() {
        // With only one slot of each kind, another instance cannot run if the failed one is retained.
        let mut pool = wasmtime::PoolingAllocationConfig::default();
        pool.total_core_instances(1)
            .total_memories(1)
            .total_tables(0);
        let engine = Engine::new(config().allocation_strategy(pool)).unwrap();
        let module = Module::new(
            &engine,
            r#"(module
            (memory 1)
            (func (export "run") (param $n i32) (result i32)
                i32.const 0 i32.load
                i32.const 0 i32.const 42 i32.store
                (loop $count local.get $n i32.const 1 i32.sub local.tee $n br_if $count)))"#,
        )
        .unwrap();
        let outcome = call_module(&module, "run", 100, 30);
        assert!(
            matches!(outcome, Err(EonError::ResourceExhausted { ref resource, requested: 30, available: 0 }) if resource == "wasm_fuel"),
            "{outcome:?}"
        );
        assert_eq!(call_module(&module, "run", 100, 10_000).unwrap(), 0);
    }

    #[test]
    fn pure_dispatch_reclaims_its_only_slot_on_trap_and_rejected_output() {
        let mut pool = wasmtime::PoolingAllocationConfig::default();
        pool.total_core_instances(1)
            .total_memories(1)
            .total_tables(0);
        let engine = Engine::new(config().allocation_strategy(pool)).unwrap();
        let tool = PureModule(
            Module::new(
                &engine,
                r#"(module (memory (export "memory") 1)
            (func (export "run") (param i32) (result i32) (local $input i32)
                i32.const 0 i32.load8_u local.set $input
                ;; Return 'A' only if the previous instance's marker was cleared.
                i32.const 0 i32.const 512 i32.load8_u i32.const 65 i32.add i32.store8
                i32.const 512 i32.const 1 i32.store8
                local.get $input i32.const 49 i32.eq if (loop br 0) end
                local.get $input i32.const 50 i32.eq if i32.const -1 return end
                i32.const 1))"#,
            )
            .unwrap(),
        );
        for (input, failure) in [("1", "wasm_fuel"), ("2", "tool_output_bytes")] {
            assert!(
                matches!(tool.run(input), Err(EonError::ResourceExhausted {resource, ..}) if resource == failure)
            );
            assert_eq!(tool.run("0").unwrap(), "A");
        }
    }

    #[test]
    fn fuel_in_the_start_function_is_charged_before_export_lookup() {
        let module = br#"(module (func $init) (start $init)
            (func (export "run") (param i32) (result i32) local.get 0))"#;
        assert!(matches!(
            call_i32_export(module, "absent", 0, 0),
            Err(EonError::ResourceExhausted { ref resource, .. }) if resource == "wasm_fuel"
        ));
        assert_eq!(call_i32_export(module, "run", 42, 100).unwrap(), 42);
    }

    #[test]
    fn linear_memory_limit_applies_to_initial_size_and_growth() {
        let grow = br#"(module (memory 1)
            (func (export "run") (param i32) (result i32) local.get 0 memory.grow))"#;
        // The guest starts with one 64 KiB page. Wasm growth returns the old size or -1 on denial.
        assert_eq!(call_i32_export(grow, "run", 1023, u64::MAX).unwrap(), 1);
        assert_eq!(call_i32_export(grow, "run", 1024, u64::MAX).unwrap(), -1);
        assert!(matches!(
            call_i32_export(b"(module (memory 1025))", "absent", 0, 100),
            Err(EonError::OperationFailed { ref operation, .. }) if operation == "wasm.instantiate"
        ));
        for unsupported in [
            "(module (memory 1) (memory 1))",
            "(module (memory 1 1 shared))",
        ] {
            assert!(matches!(
                call_i32_export(unsupported.as_bytes(), "absent", 0, 100),
                Err(EonError::OperationFailed { ref operation, .. }) if operation == "wasm.compile"
            ));
        }
    }

    #[test]
    fn a_missing_export_is_reported_not_guessed() {
        assert!(matches!(
            call_i32_export(ADD_TWO.as_bytes(), "nope", 1, 1_000_000),
            Err(EonError::OperationFailed { ref operation, .. }) if operation == "wasm.export"
        ));
    }
}
