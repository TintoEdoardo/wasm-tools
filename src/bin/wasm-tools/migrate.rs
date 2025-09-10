use anyhow::Result;
use clap::Parser;
use wasm_migrate::WasmMigrate;

/// Insert checkpoint and restore procedures.
#[derive(Parser)]
pub struct Opts {
    func_index: String,

    #[clap(flatten)]
    io: wasm_tools::InputOutput,

    // #[clap(flatten)]
    // wasm_migrate: WasmMigrate<'static>,
}

impl Opts {
    pub fn general_opts(&self) -> &wasm_tools::GeneralOpts {
        self.io.general_opts()
    }

    pub fn run(&self) -> Result<()> {
        let wasm = self.io.parse_input_wasm()?;

        // Prerequisites for the predicate.
        anyhow::ensure!(
            self.func_index.parse::<i32>().is_ok(),
            "The func_index '{}' is not a valid number.",
            self.func_index
        );

        let mut migration_injector: WasmMigrate = WasmMigrate::new();
        // For now, we are considering just core modules.
        migration_injector.component_index = 0;
        migration_injector.core_module_index = 0;
        migration_injector.func_index = self.func_index.parse::<i32>()?;

        // Inject the procedures into the input wasm bytes.
        let wasm_with_cr =
            migration_injector.compute(&wasm, false);

        // Then print the resulting module.
        let config = wasmprinter::Config::new();
        self.io.output(wasm_tools::Output::Wat {
            wasm: &wasm_with_cr,
            config,
        })
    }
}
