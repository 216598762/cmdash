//! Opt-in WebAssembly plugin isolation.
//!
//! The default build does not include Wasmtime. When `wasm-plugins` is enabled,
//! this host validates modules, rejects imports until an explicit host-function
//! ABI is finalized, and instantiates each module in its own store. No terminal
//! handles, filesystem access, or WASI capabilities are exposed.

use wasmtime::{Config, Engine, Instance, Linker, Module, Store};

pub const WASM_PLUGIN_RUNTIME: &str = "wasm";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WasmLimits {
    pub max_module_bytes: usize,
    pub max_fuel: u64,
}

impl Default for WasmLimits {
    fn default() -> Self {
        Self {
            max_module_bytes: 4 * 1024 * 1024,
            max_fuel: 5_000_000,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WasmPluginError {
    #[error("WASM module is {actual} bytes; limit is {limit}")]
    ModuleTooLarge { limit: usize, actual: usize },
    #[error("could not create WASM engine: {0}")]
    Engine(String),
    #[error("WASM module rejected: {0}")]
    Module(String),
    #[error("WASM imports are not allowed yet: {0}")]
    ImportsNotAllowed(String),
    #[error("WASM plugin instantiation failed: {0}")]
    Instantiation(String),
}

pub struct WasmPluginHost {
    engine: Engine,
    module: Module,
    limits: WasmLimits,
}

impl WasmPluginHost {
    pub fn load(bytes: &[u8], limits: WasmLimits) -> Result<Self, WasmPluginError> {
        if bytes.len() > limits.max_module_bytes {
            return Err(WasmPluginError::ModuleTooLarge {
                limit: limits.max_module_bytes,
                actual: bytes.len(),
            });
        }

        let mut config = Config::new();
        config.consume_fuel(true);
        let engine =
            Engine::new(&config).map_err(|error| WasmPluginError::Engine(error.to_string()))?;
        let module = Module::from_binary(&engine, bytes)
            .map_err(|error| WasmPluginError::Module(error.to_string()))?;
        if let Some(import) = module.imports().next() {
            return Err(WasmPluginError::ImportsNotAllowed(format!(
                "{}::{}",
                import.module(),
                import.name()
            )));
        }

        Ok(Self {
            engine,
            module,
            limits,
        })
    }

    pub fn limits(&self) -> WasmLimits {
        self.limits
    }

    pub fn exported_names(&self) -> impl Iterator<Item = &str> {
        self.module.exports().map(|export| export.name())
    }

    pub fn instantiate(&self) -> Result<WasmPluginInstance, WasmPluginError> {
        let mut store = Store::new(&self.engine, ());
        store
            .set_fuel(self.limits.max_fuel)
            .map_err(|error| WasmPluginError::Instantiation(error.to_string()))?;
        let linker = Linker::new(&self.engine);
        let instance = linker
            .instantiate(&mut store, &self.module)
            .map_err(|error| WasmPluginError::Instantiation(error.to_string()))?;
        Ok(WasmPluginInstance { store, instance })
    }
}

pub struct WasmPluginInstance {
    store: Store<()>,
    instance: Instance,
}

impl WasmPluginInstance {
    pub fn has_export(&mut self, name: &str) -> bool {
        self.instance.get_func(&mut self.store, name).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY_MODULE: &[u8] = b"\0asm\x01\0\0\0";

    #[test]
    fn validates_and_instantiates_an_import_free_module() {
        let host = WasmPluginHost::load(EMPTY_MODULE, WasmLimits::default()).unwrap();
        let mut instance = host.instantiate().unwrap();

        assert_eq!(host.exported_names().count(), 0);
        assert!(!instance.has_export("cmdash_render"));
    }

    #[test]
    fn rejects_modules_that_exceed_the_host_budget() {
        let error = WasmPluginHost::load(
            EMPTY_MODULE,
            WasmLimits {
                max_module_bytes: 1,
                max_fuel: 1,
            },
        )
        .err()
        .unwrap();
        assert!(matches!(error, WasmPluginError::ModuleTooLarge { .. }));
    }
}
